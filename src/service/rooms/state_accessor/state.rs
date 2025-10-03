use std::{borrow::Borrow, ops::Deref, sync::Arc};

use futures::{FutureExt, Stream, StreamExt, TryFutureExt, future::try_join, pin_mut};
use ruma::{
	EventId, OwnedEventId, UserId,
	events::{
		StateEventType,
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
		stream::{BroadbandExt, IterStream, ReadyExt, TryIgnore},
	},
};

use crate::rooms::{
	short::{ShortEventId, ShortStateHash, ShortStateKey},
	state_compressor::{CompressedState, compress_state_event, parse_compressed_state_event},
};

/// The user was a joined member at this state (potentially in the past)
#[implement(super::Service)]
#[inline]
pub async fn user_was_joined(&self, shortstatehash: ShortStateHash, user_id: &UserId) -> bool {
	self.user_membership(shortstatehash, user_id)
		.await == MembershipState::Join
}

/// The user was an invited or joined room member at this state (potentially
/// in the past)
#[implement(super::Service)]
#[inline]
pub async fn user_was_invited(&self, shortstatehash: ShortStateHash, user_id: &UserId) -> bool {
	let s = self
		.user_membership(shortstatehash, user_id)
		.await;
	s == MembershipState::Join || s == MembershipState::Invite
}

/// Get membership for given user in state
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

/// Returns a single PDU from `room_id` with key (`event_type`,`state_key`).
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

/// Returns a single PDU from `room_id` with key (`event_type`,
/// `state_key`).
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

/// Returns a single EventId from `room_id` with key (`event_type`,
/// `state_key`).
#[implement(super::Service)]
pub async fn state_get_id<Id>(
	&self,
	shortstatehash: ShortStateHash,
	event_type: &StateEventType,
	state_key: &str,
) -> Result<Id>
where
	Id: for<'de> Deserialize<'de> + Send + Sized + ToOwned,
	<Id as ToOwned>::Owned: Borrow<EventId>,
{
	let shorteventid = self
		.state_get_shortid(shortstatehash, event_type, state_key)
		.await?;

	self.services
		.short
		.get_eventid_from_short(shorteventid)
		.await
}

/// Returns a single EventId from `room_id` with key (`event_type`,
/// `state_key`).
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

/// Iterates the state_keys for an event_type in the state; current state
/// event_id included.
#[implement(super::Service)]
pub fn state_keys_with_ids<'a, Id>(
	&'a self,
	shortstatehash: ShortStateHash,
	event_type: &'a StateEventType,
) -> impl Stream<Item = (StateKey, Id)> + Send + 'a
where
	Id: for<'de> Deserialize<'de> + Send + Sized + ToOwned + 'a,
	<Id as ToOwned>::Owned: Borrow<EventId>,
{
	let state_keys_with_short_ids = self
		.state_keys_with_shortids(shortstatehash, event_type)
		.unzip()
		.map(|(ssks, sids): (Vec<StateKey>, Vec<u64>)| (ssks, sids))
		.shared();

	let state_keys = state_keys_with_short_ids
		.clone()
		.map(at!(0))
		.map(Vec::into_iter)
		.map(IterStream::stream)
		.flatten_stream();

	let shorteventids = state_keys_with_short_ids
		.map(at!(1))
		.map(Vec::into_iter)
		.map(IterStream::stream)
		.flatten_stream();

	self.services
		.short
		.multi_get_eventid_from_short(shorteventids)
		.zip(state_keys)
		.ready_filter_map(|(eid, sk)| eid.map(move |eid| (sk, eid)).ok())
}

/// Iterates the state_keys for an event_type in the state; current state
/// event_id included.
#[implement(super::Service)]
pub fn state_keys_with_shortids<'a>(
	&'a self,
	shortstatehash: ShortStateHash,
	event_type: &'a StateEventType,
) -> impl Stream<Item = (StateKey, ShortEventId)> + Send + 'a {
	let short_ids = self
		.state_full_shortids(shortstatehash)
		.ignore_err()
		.unzip()
		.map(|(ssks, sids): (Vec<u64>, Vec<u64>)| (ssks, sids))
		.boxed()
		.shared();

	let shortstatekeys = short_ids
		.clone()
		.map(at!(0))
		.map(Vec::into_iter)
		.map(IterStream::stream)
		.flatten_stream();

	let shorteventids = short_ids
		.map(at!(1))
		.map(Vec::into_iter)
		.map(IterStream::stream)
		.flatten_stream();

	self.services
		.short
		.multi_get_statekey_from_short(shortstatekeys)
		.zip(shorteventids)
		.ready_filter_map(|(res, id)| res.map(|res| (res, id)).ok())
		.ready_filter_map(move |((event_type_, state_key), event_id)| {
			event_type_
				.eq(event_type)
				.then_some((state_key, event_id))
		})
}

/// Iterates the state_keys for an event_type in the state
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

/// Returns the state events removed between the interval (present in .0 but
/// not in .1)
#[implement(super::Service)]
#[inline]
pub fn state_removed(
	&self,
	shortstatehash: pair_of!(ShortStateHash),
) -> impl Stream<Item = (ShortStateKey, ShortEventId)> + Send + '_ {
	self.state_added((shortstatehash.1, shortstatehash.0))
}

/// Returns the state events added between the interval (present in .1 but
/// not in .0)
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

/// Builds a StateMap by iterating over all keys that start
/// with state_hash, this gives the full state for the given state_hash.
#[implement(super::Service)]
pub fn state_full_ids<'a, Id>(
	&'a self,
	shortstatehash: ShortStateHash,
) -> impl Stream<Item = (ShortStateKey, Id)> + Send + 'a
where
	Id: for<'de> Deserialize<'de> + Send + Sized + ToOwned + 'a,
	<Id as ToOwned>::Owned: Borrow<EventId>,
{
	let shortids = self
		.state_full_shortids(shortstatehash)
		.ignore_err()
		.unzip()
		.shared();

	let shortstatekeys = shortids
		.clone()
		.map(at!(0))
		.map(Vec::into_iter)
		.map(IterStream::stream)
		.flatten_stream();

	let shorteventids = shortids
		.map(at!(1))
		.map(Vec::into_iter)
		.map(IterStream::stream)
		.flatten_stream();

	self.services
		.short
		.multi_get_eventid_from_short(shorteventids)
		.zip(shortstatekeys)
		.ready_filter_map(|(event_id, shortstatekey)| Some((shortstatekey, event_id.ok()?)))
}

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
		.boxed()
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
