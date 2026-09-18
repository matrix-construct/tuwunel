//! Reads events and short IDs from a historical room-state snapshot.
//!
//! Strict streams preserve snapshot and reverse-mapping errors. The remaining
//! collection streams are intentionally best effort and omit unresolved entries.

use std::{ops::Deref, sync::Arc};

use futures::{
	FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt, future::try_join, pin_mut,
};
use ruma::{
	OwnedEventId, UserId,
	events::{
		StateEventType, TimelineEventType,
		room::member::{MembershipState, RoomMemberEventContent},
	},
};
use serde::Deserialize;
use tuwunel_core::{
	Result, at, err, implement,
	matrix::{Event, Pdu, StateKey},
	pair_of,
	utils::{
		result::FlatOk,
		stream::{BroadbandExt, IterStream, ReadyExt, TryBroadbandExt, TryIgnore, TryTools},
	},
};

use crate::rooms::{
	short::{ShortEventId, ShortStateHash, ShortStateKey},
	state_compressor::{CompressedState, compress_state_event, parse_compressed_state_event},
};

/// Reports whether a user was joined in a selected state snapshot.
///
/// Missing or invalid membership state is treated as `leave`, so lookup errors
/// return `false`.
#[implement(super::Service)]
#[inline]
pub async fn user_was_joined(&self, shortstatehash: ShortStateHash, user_id: &UserId) -> bool {
	self.user_membership(shortstatehash, user_id)
		.await == MembershipState::Join
}

/// Reports whether a user was invited or joined in a selected state snapshot.
///
/// Missing or invalid membership state is treated as `leave`, so lookup errors
/// return `false`.
#[implement(super::Service)]
#[inline]
pub async fn user_was_invited(&self, shortstatehash: ShortStateHash, user_id: &UserId) -> bool {
	let s = self
		.user_membership(shortstatehash, user_id)
		.await;
	s == MembershipState::Join || s == MembershipState::Invite
}

/// Returns a user's membership in a selected state snapshot.
///
/// Missing state, unavailable events, and invalid membership content all fall
/// back to [`MembershipState::Leave`].
#[implement(super::Service)]
pub async fn user_membership(
	&self,
	shortstatehash: ShortStateHash,
	user_id: &UserId,
) -> MembershipState {
	self.state_get_content(shortstatehash, &StateEventType::RoomMember, user_id.as_str())
		.await
		.map_or(MembershipState::Leave, |c: RoomMemberEventContent| c.membership)
}

/// MSC4115: the user's room membership "just after" the given PDU landed.
///
/// `pdu_shortstatehash` returns state-before-the-event, so a member event
/// targeting `user_id` overrides that lookup with its own content. Missing or
/// invalid state falls back to [`MembershipState::Leave`].
#[implement(super::Service)]
pub async fn user_membership_at_pdu(&self, user_id: &UserId, pdu: &Pdu) -> MembershipState {
	if pdu.kind() == &TimelineEventType::RoomMember
		&& pdu.state_key() == Some(user_id.as_str())
		&& let Ok(content) = pdu.get_content::<RoomMemberEventContent>()
	{
		return content.membership;
	}

	let Ok(shortstatehash) = self
		.services
		.state
		.pdu_shortstatehash(pdu.event_id())
		.await
	else {
		return MembershipState::Leave;
	};

	self.user_membership(shortstatehash, user_id)
		.await
}

/// Deserializes one event's content from a selected state snapshot.
///
/// The event is selected by `(event_type, state_key)`. State, short-ID,
/// timeline, and content errors are returned to the caller.
#[implement(super::Service)]
pub async fn state_get_content<T>(
	&self,
	shortstatehash: ShortStateHash,
	event_type: &StateEventType,
	state_key: &str,
) -> Result<T>
where
	T: for<'de> Deserialize<'de> + Send,
{
	self.state_get(shortstatehash, event_type, state_key)
		.await
		.and_then(|event| event.get_content())
}

/// Reports whether a state snapshot contains one state tuple.
///
/// Failure to resolve the tuple's short state key or load the snapshot is
/// treated as absence.
#[implement(super::Service)]
pub async fn state_contains(
	&self,
	shortstatehash: ShortStateHash,
	event_type: &StateEventType,
	state_key: &str,
) -> bool {
	let Ok(shortstatekey) = self
		.services
		.short
		.get_shortstatekey(event_type, state_key)
		.await
	else {
		return false;
	};

	self.state_contains_shortstatekey(shortstatehash, shortstatekey)
		.await
}

/// Reports whether a state snapshot contains any event of one type.
///
/// Snapshot and state-key mapping errors are omitted by the underlying
/// best-effort stream and can therefore produce `false`.
#[implement(super::Service)]
pub async fn state_contains_type(
	&self,
	shortstatehash: ShortStateHash,
	event_type: &StateEventType,
) -> bool {
	let state_keys = self.state_keys(shortstatehash, event_type);

	pin_mut!(state_keys);
	state_keys.next().await.is_some()
}

/// Reports whether a snapshot contains a short state key.
///
/// The compressed snapshot is searched across every short event ID for the
/// key. Failure to load the snapshot is treated as absence.
#[implement(super::Service)]
pub async fn state_contains_shortstatekey(
	&self,
	shortstatehash: ShortStateHash,
	shortstatekey: ShortStateKey,
) -> bool {
	let start = compress_state_event(shortstatekey, 0);
	let end = compress_state_event(shortstatekey, u64::MAX);

	self.load_full_state(shortstatehash)
		.map_ok(|full_state| full_state.range(start..=end).next().copied())
		.await
		.flat_ok()
		.is_some()
}

/// Returns one PDU from a selected state snapshot.
///
/// The event is selected by `(event_type, state_key)`. Short-ID and timeline
/// lookup failures are returned to the caller.
#[implement(super::Service)]
pub async fn state_get(
	&self,
	shortstatehash: ShortStateHash,
	event_type: &StateEventType,
	state_key: &str,
) -> Result<Pdu> {
	let event_id: OwnedEventId = self
		.state_get_id(shortstatehash, event_type, state_key)
		.await?;

	self.services.timeline.get_pdu(&event_id).await
}

/// Returns one event ID from a selected state snapshot.
///
/// Both the state tuple's short key and its short event ID must resolve.
#[implement(super::Service)]
pub async fn state_get_id(
	&self,
	shortstatehash: ShortStateHash,
	event_type: &StateEventType,
	state_key: &str,
) -> Result<OwnedEventId> {
	let shorteventid = self
		.state_get_shortid(shortstatehash, event_type, state_key)
		.await?;

	self.services
		.short
		.get_eventid_from_short(shorteventid)
		.await
}

/// Returns one short event ID from a selected state snapshot.
///
/// The method resolves `(event_type, state_key)` to a short state key and
/// searches the compressed snapshot. An absent tuple is returned as not found.
#[implement(super::Service)]
pub async fn state_get_shortid(
	&self,
	shortstatehash: ShortStateHash,
	event_type: &StateEventType,
	state_key: &str,
) -> Result<ShortEventId> {
	let shortstatekey = self
		.services
		.short
		.get_shortstatekey(event_type, state_key)
		.await?;

	let start = compress_state_event(shortstatekey, 0);
	let end = compress_state_event(shortstatekey, u64::MAX);
	self.load_full_state(shortstatehash)
		.map_ok(|full_state| {
			full_state
				.range(start..=end)
				.next()
				.copied()
				.map(parse_compressed_state_event)
				.map(at!(1))
				.ok_or(err!(Request(NotFound("Not found in room state"))))
		})
		.await?
}

/// Streams resolvable events of one type from a state snapshot.
///
/// Snapshot, short-ID, and timeline lookup failures are skipped, so this is a
/// best-effort view rather than a completeness guarantee.
#[implement(super::Service)]
pub fn state_type_pdus<'a>(
	&'a self,
	shortstatehash: ShortStateHash,
	event_type: &'a StateEventType,
) -> impl Stream<Item = impl Event> + Send + 'a {
	self.state_keys_with_ids(shortstatehash, event_type)
		.map(at!(1))
		.broad_filter_map(async |event_id: OwnedEventId| {
			self.services
				.timeline
				.get_pdu(&event_id)
				.await
				.ok()
		})
}

/// Streams state keys and event IDs for one type in a snapshot.
///
/// Snapshot and reverse-mapping failures are skipped. The stream buffers the
/// selected short IDs before resolving event IDs in a batch.
#[implement(super::Service)]
pub fn state_keys_with_ids<'a>(
	&'a self,
	shortstatehash: ShortStateHash,
	event_type: &'a StateEventType,
) -> impl Stream<Item = (StateKey, OwnedEventId)> + Send + 'a {
	self.state_keys_with_shortids(shortstatehash, event_type)
		.unzip()
		.map(|(state_keys, shorteventids): (Vec<_>, Vec<_>)| {
			self.services
				.short
				.multi_get_eventid_from_short(shorteventids.into_iter().stream())
				.zip(state_keys.into_iter().stream())
				.ready_filter_map(|(eid, sk)| eid.map(move |eid| (sk, eid)).ok())
		})
		.flatten_stream()
}

/// Streams state keys and short event IDs for one type in a snapshot.
///
/// Snapshot and short-state-key mapping failures are skipped. The full
/// compressed snapshot is buffered before filtering by event type.
#[implement(super::Service)]
pub fn state_keys_with_shortids<'a>(
	&'a self,
	shortstatehash: ShortStateHash,
	event_type: &'a StateEventType,
) -> impl Stream<Item = (StateKey, ShortEventId)> + Send + 'a {
	self.state_full_shortids(shortstatehash)
		.ignore_err()
		.unzip()
		.map(move |(shortstatekeys, shorteventids): (Vec<_>, Vec<_>)| {
			self.services
				.short
				.multi_get_statekey_from_short(shortstatekeys.into_iter().stream())
				.zip(shorteventids.into_iter().stream())
				.ready_filter_map(|(res, id)| res.map(|res| (res, id)).ok())
				.ready_filter_map(move |((event_type_, state_key), event_id)| {
					event_type_
						.eq(event_type)
						.then_some((state_key, event_id))
				})
		})
		.flatten_stream()
}

/// Streams state keys for one event type in a snapshot.
///
/// Snapshot and short-state-key mapping failures are skipped, so the stream is
/// best effort.
#[implement(super::Service)]
pub fn state_keys<'a>(
	&'a self,
	shortstatehash: ShortStateHash,
	event_type: &'a StateEventType,
) -> impl Stream<Item = StateKey> + Send + 'a {
	let short_ids = self
		.state_full_shortids(shortstatehash)
		.ignore_err()
		.map(at!(0));

	self.services
		.short
		.multi_get_statekey_from_short(short_ids)
		.ready_filter_map(Result::ok)
		.ready_filter_map(move |(event_type_, state_key)| {
			event_type_.eq(event_type).then_some(state_key)
		})
}

/// Streams state entries removed between two snapshots.
///
/// Entries present in the first hash and absent from the second are returned.
/// Failure to load either snapshot produces an empty stream.
#[implement(super::Service)]
#[inline]
pub fn state_removed(
	&self,
	shortstatehash: pair_of!(ShortStateHash),
) -> impl Stream<Item = (ShortStateKey, ShortEventId)> + Send + '_ {
	self.state_added((shortstatehash.1, shortstatehash.0))
}

/// Streams state entries added between two snapshots.
///
/// Entries absent from the first hash and present in the second are returned.
/// Failure to load either snapshot produces an empty stream.
#[implement(super::Service)]
pub fn state_added(
	&self,
	shortstatehash: pair_of!(ShortStateHash),
) -> impl Stream<Item = (ShortStateKey, ShortEventId)> + Send + '_ {
	let a = self.load_full_state(shortstatehash.0);
	let b = self.load_full_state(shortstatehash.1);
	try_join(a, b)
		.map_ok(|(a, b)| b.difference(&a).copied().collect::<Vec<_>>())
		.map_ok(IterStream::try_stream)
		.try_flatten_stream()
		.ignore_err()
		.map(parse_compressed_state_event)
}

/// Streams resolvable keyed events from a state snapshot.
///
/// Events without a state key and entries with failed short-ID or timeline
/// lookups are omitted by the underlying best-effort PDU stream.
#[implement(super::Service)]
pub fn state_full(
	&self,
	shortstatehash: ShortStateHash,
) -> impl Stream<Item = ((StateEventType, StateKey), impl Event)> + Send + '_ {
	self.state_full_pdus(shortstatehash)
		.ready_filter_map(|pdu| {
			Some(((pdu.kind().to_cow_str().into(), pdu.state_key()?.into()), pdu))
		})
}

/// Streams every resolvable PDU from a state snapshot.
///
/// Snapshot, reverse-mapping, and timeline failures are silently skipped. Use
/// [`Self::state_full_pdus_strict`] when completeness is required.
#[implement(super::Service)]
pub fn state_full_pdus(
	&self,
	shortstatehash: ShortStateHash,
) -> impl Stream<Item = impl Event> + Send + '_ {
	let short_ids = self
		.state_full_shortids(shortstatehash)
		.ignore_err()
		.map(at!(1));

	self.services
		.short
		.multi_get_eventid_from_short(short_ids)
		.ready_filter_map(Result::ok)
		.broad_filter_map(async |event_id: OwnedEventId| {
			self.services
				.timeline
				.get_pdu(&event_id)
				.await
				.ok()
		})
}

/// Streams every PDU in a state snapshot while preserving errors.
///
/// Snapshot and reverse-mapping failures are emitted before any partial ID map.
/// Timeline lookup failures are yielded for their individual entries.
#[implement(super::Service)]
pub fn state_full_pdus_strict(
	&self,
	shortstatehash: ShortStateHash,
) -> impl Stream<Item = Result<impl Event>> + Send + '_ {
	self.state_full_ids_strict(shortstatehash)
		.broad_and_then(async |(_, event_id)| self.services.timeline.get_pdu(&event_id).await)
}

/// Streams short state keys and resolvable event IDs from a snapshot.
///
/// Snapshot and reverse-mapping failures are skipped, so this best-effort
/// stream can be partial. Use [`Self::state_full_ids_strict`] for completeness.
#[implement(super::Service)]
pub fn state_full_ids(
	&self,
	shortstatehash: ShortStateHash,
) -> impl Stream<Item = (ShortStateKey, OwnedEventId)> + Send + '_ {
	self.state_full_shortids(shortstatehash)
		.ignore_err()
		.unzip()
		.map(|(shortstatekeys, shorteventids): (Vec<_>, Vec<_>)| {
			self.services
				.short
				.multi_get_eventid_from_short(shorteventids.into_iter().stream())
				.zip(shortstatekeys.into_iter().stream())
				.ready_filter_map(|(eid, ssk)| eid.ok().map(|eid| (ssk, eid)))
		})
		.flatten_stream()
}

/// Streams a complete short-state-key to event-ID map for a snapshot.
///
/// Snapshot and reverse-mapping failures are returned without yielding a
/// partial map.
#[implement(super::Service)]
pub fn state_full_ids_strict(
	&self,
	shortstatehash: ShortStateHash,
) -> impl Stream<Item = Result<(ShortStateKey, OwnedEventId)>> + Send + '_ {
	self.state_full_shortids(shortstatehash)
		.try_unzip::<Vec<_>, Vec<_>>()
		.and_then(async move |(shortstatekeys, shorteventids)| {
			self.services
				.short
				.multi_get_eventid_from_short(shorteventids.into_iter().stream())
				.zip(shortstatekeys.into_iter().stream())
				.map(|(event_id, shortstatekey)| {
					event_id.map(|event_id| (shortstatekey, event_id))
				})
				.try_collect::<Vec<_>>()
				.await
		})
		.map_ok(Vec::into_iter)
		.map_ok(IterStream::try_stream)
		.try_flatten_stream()
}

/// Streams every compressed `(short state key, short event ID)` pair.
///
/// A snapshot-load failure is yielded as an error. Once loaded, the immutable
/// compressed state is copied into the stream without further lookups.
#[implement(super::Service)]
pub fn state_full_shortids(
	&self,
	shortstatehash: ShortStateHash,
) -> impl Stream<Item = Result<(ShortStateKey, ShortEventId)>> + Send + '_ {
	self.load_full_state(shortstatehash)
		.map_ok(|full_state| {
			full_state
				.deref()
				.iter()
				.copied()
				.map(parse_compressed_state_event)
				.collect()
		})
		.map_ok(Vec::into_iter)
		.map_ok(IterStream::try_stream)
		.try_flatten_stream()
}

#[implement(super::Service)]
#[tracing::instrument(name = "load", level = "debug", skip(self))]
async fn load_full_state(&self, shortstatehash: ShortStateHash) -> Result<Arc<CompressedState>> {
	self.services
		.state_compressor
		.load_shortstatehash_info(shortstatehash)
		.map_err(|e| err!(Database("Missing state IDs: {e}")))
		.map_ok(|vec| {
			vec.last()
				.expect("at least one layer")
				.full_state
				.clone()
		})
		.await
}
