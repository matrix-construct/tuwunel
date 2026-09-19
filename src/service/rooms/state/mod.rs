//! Stores room state snapshots and tracks each room's forward extremities.
//!
//! The service associates events with compressed state hashes and replays
//! derived cache effects when state is forced. State snapshots themselves are
//! encoded and reconstructed by the state compressor service.

mod fetch_state;
mod prune;

use std::{collections::HashMap, fmt::Write, iter::once, sync::Arc};

use async_trait::async_trait;
/// Fetches state events through a held map of short state keys.
///
/// Sibling services share its distinction between absent state and missing storage.
pub(crate) use fetch_state::IdMapState;
use futures::{FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt, future::join_all};
/// Re-exports the receive-path pruning goal calculation within the crate.
///
/// Sibling room services use it to pace extremity reduction.
pub(crate) use prune::prune_goal;
/// Re-exports the forward-extremity pruning result and invocation source.
///
/// Callers use these types to report pruning effects and select path-specific
/// behavior.
pub use prune::{PruneSummary, Trigger};
use ruma::{
	CanonicalJsonObject, EventId, OwnedEventId, OwnedRoomId, RoomId, RoomVersionId, UserId,
	events::{
		AnyStrippedStateEvent, StateEventType, TimelineEventType,
		room::member::{MembershipState, RoomMemberEventContent},
	},
	room_version_rules::AuthorizationRules,
	serde::Raw,
};
use serde_json::value::RawValue as RawJsonValue;
use tuwunel_core::{
	Event, PduEvent, Result, err,
	error::inspect_debug_log,
	implement,
	matrix::{PduCount, RoomVersionRules, StateKey, TypeStateKey, room_version},
	result::{AndThenRef, FlatOk, NotFound},
	smallvec::SmallVec,
	trace,
	utils::{
		BoolExt, IterStream, MutexMap, MutexMapGuard, ReadyExt, TryReadyExt, calculate_hash,
		mutex_map::Guard,
		stream::{TryBroadbandExt, TryIgnore, WidebandExt},
	},
	warn,
};
use tuwunel_database::{Deserialized, Ignore, Interfix, Map, Txn};

use crate::{
	rooms::{
		short::{ShortEventId, ShortStateHash, ShortStateKey},
		state_cache::{MembershipUpdate, StrippedRoomState},
		state_compressor::{CompressedState, parse_compressed_state_event},
		state_res::{StateMap, auth_types_for_event},
	},
	services::OnceServices,
};

/// Manages current room state, event state snapshots, and forward extremities.
///
/// State mutations are serialized per room and delegated to the compressor for
/// persistent delta encoding. The service also coordinates cache updates that
/// follow forced state changes.
pub struct Service {
	/// Serializes room state as the middle per-room operation.
	///
	/// Acquire it after federation and before timeline insertion when those
	/// mutexes share a room. Never acquire the federation mutex while holding
	/// this guard.
	pub mutex: RoomMutexMap,
	services: Arc<OnceServices>,
	db: Data,
}

struct Data {
	shorteventid_shortstatehash: Arc<Map>,
	roomid_shortstatehash: Arc<Map>,
	roomid_pduleaves: Arc<Map>,
}

type RoomMutexMap = MutexMap<OwnedRoomId, ()>;
/// Guard proving exclusive access to a room's state mutation path.
///
/// Acquire it after the federation guard and before the timeline insertion
/// guard when the same operation needs all three.
pub type RoomMutexGuard = MutexMapGuard<OwnedRoomId, ()>;
type ForwardExtremities = SmallVec<[OwnedEventId; 1]>;

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			mutex: RoomMutexMap::new(),
			services: args.services.clone(),
			db: Data {
				shorteventid_shortstatehash: args.db["shorteventid_shortstatehash"].clone(),
				roomid_shortstatehash: args.db["roomid_shortstatehash"].clone(),
				roomid_pduleaves: args.db["roomid_pduleaves"].clone(),
			},
		}))
	}

	async fn memory_usage(&self, out: &mut (dyn Write + Send)) -> Result {
		let mutex = self.mutex.len();
		writeln!(out, "- state_mutex: {mutex}")?;

		Ok(())
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Forces a room to use an existing state snapshot.
///
/// Resolvable membership additions replay their cache effects before the
/// current state hash is installed. Reverse-ID and PDU lookup failures are
/// skipped, while a membership-effect error returns before the state hash
/// changes. Joined counts are refreshed and the cached space summary is
/// invalidated while the caller retains the room state guard.
#[implement(Service)]
#[tracing::instrument(
	name = "force",
	level = "debug",
	skip_all,
	fields(
		count = ?self.services.globals.pending_count(),
		%shortstatehash,
	)
)]
pub async fn force_state(
	&self,
	room_id: &RoomId,
	shortstatehash: u64,
	statediffnew: Arc<CompressedState>,
	_statediffremoved: Arc<CompressedState>,
	state_lock: &RoomMutexGuard,
) -> Result {
	statediffnew
		.iter()
		.stream()
		.map(|&new| parse_compressed_state_event(new).1)
		.wide_filter_map(async |shorteventid| {
			let event_id: OwnedEventId = self
				.services
				.short
				.get_eventid_from_short(shorteventid)
				.inspect_err(inspect_debug_log)
				.await
				.ok()?;

			self.services
				.timeline
				.get_pdu(&event_id)
				.await
				.ok()
		})
		.map(Ok)
		.try_for_each(async |pdu| match pdu.kind {
			| TimelineEventType::RoomMember => self.force_member_effects(room_id, &pdu).await,
			| _ => Ok(()),
		})
		.boxed() // size firewall
		.await?;

	self.services
		.state_cache
		.update_joined_count(room_id)
		.await;

	self.set_room_state(room_id, shortstatehash, state_lock);

	// Forced state may change this room's cached hierarchy summary.
	self.services.spaces.cache_evict(room_id);

	Ok(())
}

/// Record the membership transition a replayed `m.room.member` event carries.
///
/// A replayed invite is judged by the sender named in its stripped state, so a
/// local invitee's row has to carry one. An event whose state key or content
/// does not parse is skipped rather than failing the whole replay.
#[implement(Service)]
async fn force_member_effects(&self, room_id: &RoomId, pdu: &PduEvent) -> Result {
	let Some(user_id) = pdu
		.state_key
		.as_ref()
		.map(UserId::parse)
		.flat_ok()
	else {
		return Ok(());
	};

	let Ok(membership_event): Result<RoomMemberEventContent> = pdu.get_content() else {
		return Ok(());
	};

	let last_state = membership_event
		.membership
		.eq(&MembershipState::Invite)
		.and_is(self.services.globals.user_is_local(&user_id))
		.then_async(|| self.replayed_invite_state(room_id, &user_id, pdu))
		.map(Option::transpose)
		.map_ok(Option::flatten)
		.await?;

	let count = self.services.globals.next_count();

	self.services
		.state_cache
		.update_membership(MembershipUpdate {
			room_id,
			user_id: &user_id,
			membership_event,
			sender: &pdu.sender,
			last_state,
			invite_via: None,
			update_joined_count: false,
			count: PduCount::Normal(*count),
		})
		.await
}

/// Computes stripped state for a replayed invite, unless the row has some.
///
/// A reset reaches this before the new state is installed, so the summary built
/// here is thinner than what the invite itself stored. Returning nothing leaves
/// the stored row for `mark_as_invited` to keep. A probe that fails to read
/// the row is an error, not an absence.
#[implement(Service)]
async fn replayed_invite_state(
	&self,
	room_id: &RoomId,
	user_id: &UserId,
	pdu: &PduEvent,
) -> Result<StrippedRoomState> {
	self.services
		.state_cache
		.has_invite_state(user_id, room_id)
		.await?
		.is_false()
		.then_async(|| self.summary_stripped(pdu))
		.map(Ok)
		.await
}

/// Associates an event with a complete compressed state snapshot.
///
/// The snapshot hash is reused when known; otherwise a short state hash and a
/// delta from the room's current snapshot are stored together. This records the
/// event's state without advancing the room's current state.
#[implement(Service)]
#[tracing::instrument(
	name = "set",
	level = "debug",
	skip(self, state_ids_compressed),
	fields(
		count = ?self.services.globals.pending_count(),
	)
)]
pub async fn set_event_state(
	&self,
	event_id: &EventId,
	room_id: &RoomId,
	state_ids_compressed: Arc<CompressedState>,
) -> Result<ShortStateHash> {
	const KEY_LEN: usize = size_of::<ShortEventId>();
	const VAL_LEN: usize = size_of::<ShortStateHash>();

	let shorteventid = self
		.services
		.short
		.get_or_create_shorteventid(event_id)
		.await;

	let state_hash = calculate_hash(state_ids_compressed.iter().map(|s| &s[..]));

	if let Ok(shortstatehash) = self
		.services
		.short
		.get_shortstatehash(&state_hash)
		.await
	{
		self.db
			.shorteventid_shortstatehash
			.aput::<KEY_LEN, VAL_LEN, _, _>(shorteventid, shortstatehash);

		return Ok(shortstatehash);
	}

	let previous_shortstatehash = self.get_room_shortstatehash(room_id).await;
	let states_parents = match previous_shortstatehash {
		| Ok(p) =>
			self.services
				.state_compressor
				.load_shortstatehash_info(p)
				.await?,
		| _ => Vec::new(),
	};

	let (statediffnew, statediffremoved) = if let Some(parent_stateinfo) = states_parents.last() {
		let statediffnew: CompressedState = state_ids_compressed
			.difference(&parent_stateinfo.full_state)
			.copied()
			.collect();

		let statediffremoved: CompressedState = parent_stateinfo
			.full_state
			.difference(&state_ids_compressed)
			.copied()
			.collect();

		(Arc::new(statediffnew), Arc::new(statediffremoved))
	} else {
		(state_ids_compressed, Arc::new(CompressedState::new()))
	};

	let save_statediff = |txn: &mut Txn, shortstatehash| {
		self.services
			.state_compressor
			.save_state_from_diff(
				txn,
				shortstatehash,
				statediffnew,
				statediffremoved,
				1_000_000, // high number because no state will be based on this one
				states_parents,
			)
	};

	let (shortstatehash, _) = self
		.services
		.short
		.get_or_create_shortstatehash(&state_hash, save_statediff)
		.await?;

	self.db
		.shorteventid_shortstatehash
		.aput::<KEY_LEN, VAL_LEN, _, _>(shorteventid, shortstatehash);

	Ok(shortstatehash)
}

/// Derives the state snapshot produced by appending a local PDU.
///
/// The event is associated with the preceding snapshot before a state event
/// creates a one-entry delta and a new short state hash. A non-state event
/// retains the preceding hash, and an unchanged state event reuses it.
/// The event's short ID is allocated here if absent, which is the only
/// allocation of it on the local append path.
///
/// # Panics
///
/// Panics if a room's first event is not state-bearing or if an unchanged state
/// entry is found without a preceding room snapshot.
#[implement(Service)]
#[tracing::instrument(
	name = "set",
	level = "debug",
	skip(self, new_pdu),
	fields(
		count = ?self.services.globals.pending_count(),
	)
)]
pub async fn append_to_state(&self, new_pdu: &PduEvent) -> Result<u64> {
	const KEY_LEN: usize = size_of::<ShortEventId>();
	const VAL_LEN: usize = size_of::<ShortStateHash>();

	let shorteventid = self
		.services
		.short
		.get_or_create_shorteventid(&new_pdu.event_id)
		.await;

	let previous_shortstatehash = self
		.get_room_shortstatehash(&new_pdu.room_id)
		.await;

	if let Ok(p) = previous_shortstatehash {
		self.db
			.shorteventid_shortstatehash
			.aput::<KEY_LEN, VAL_LEN, _, _>(shorteventid, p);
	}

	match &new_pdu.state_key {
		| Some(state_key) => {
			let states_parents = match previous_shortstatehash {
				| Ok(p) =>
					self.services
						.state_compressor
						.load_shortstatehash_info(p)
						.await?,
				| _ => Vec::new(),
			};

			let shortstatekey = self
				.services
				.short
				.get_or_create_shortstatekey(&new_pdu.kind.to_string().into(), state_key)
				.await;

			let new = self
				.services
				.state_compressor
				.compress_state_event(shortstatekey, &new_pdu.event_id)
				.await;

			let replaces = states_parents
				.last()
				.map(|info| {
					info.full_state
						.iter()
						.find(|bytes| bytes.starts_with(&shortstatekey.to_be_bytes()))
				})
				.unwrap_or_default();

			if Some(&new) == replaces {
				return Ok(previous_shortstatehash.expect("must exist"));
			}

			// TODO: statehash with deterministic inputs
			let shortstatehash = self.services.globals.next_count();
			let mut txn = self.services.db.txn();

			let mut statediffnew = CompressedState::new();
			statediffnew.insert(new);

			let mut statediffremoved = CompressedState::new();
			if let Some(replaces) = replaces {
				statediffremoved.insert(*replaces);
			}

			self.services
				.state_compressor
				.save_state_from_diff(
					&mut txn,
					*shortstatehash,
					Arc::new(statediffnew),
					Arc::new(statediffremoved),
					2,
					states_parents,
				)?;

			txn.execute();

			Ok(*shortstatehash)
		},
		| _ => Ok(previous_shortstatehash.expect("first event in room must be a state event")),
	}
}

/// Sets the room's current state hash without updating derived state caches.
///
/// The guard proves that the caller owns the room state mutation path. Callers
/// that change effective state must update the relevant caches separately.
#[implement(Service)]
#[tracing::instrument(skip(self, _mutex_lock), level = "debug")]
pub fn set_room_state(
	&self,
	room_id: &RoomId,
	shortstatehash: u64,
	// Take mutex guard to make sure users get the room state mutex
	_mutex_lock: &RoomMutexGuard,
) {
	const BUFSIZE: usize = size_of::<u64>();

	self.db
		.roomid_shortstatehash
		.raw_aput::<BUFSIZE, _, _>(room_id, shortstatehash);
}

/// Fetches the auth events required from a room's current state.
///
/// The required state keys are derived from the proposed event and room
/// authorization rules. A room without current state yields an empty map, and
/// missing short-key mappings or stored PDUs are omitted.
#[implement(Service)]
#[expect(clippy::too_many_arguments)]
#[tracing::instrument(skip(self, content), level = "debug")]
pub async fn get_auth_events(
	&self,
	room_id: &RoomId,
	kind: &TimelineEventType,
	sender: &UserId,
	state_key: Option<&str>,
	content: &serde_json::value::RawValue,
	auth_rules: &AuthorizationRules,
	include_create: bool,
) -> Result<StateMap<PduEvent>>
where
	StateEventType: Send + Sync,
	StateKey: Send + Sync,
{
	let Some(shortstatehash) = self
		.get_room_shortstatehash(room_id)
		.await
		.optional()?
	else {
		return Ok(StateMap::new());
	};

	let sauthevents: HashMap<ShortStateKey, TypeStateKey> =
		auth_types_for_event(kind, sender, state_key, content, auth_rules, include_create)?
			.into_iter()
			.try_stream()
			.broad_and_then(async |(event_type, state_key): TypeStateKey| {
				self.services
					.short
					.get_shortstatekey(&event_type, &state_key)
					.await
					.map(|sstatekey| (sstatekey, (event_type, state_key)))
					.optional()
			})
			.ready_try_filter_map(Result::Ok)
			.try_collect()
			.await?;

	let matching_state: Vec<_> = self
		.services
		.state_accessor
		.state_full_shortids(shortstatehash)
		.ready_try_filter_map(|(shortstatekey, shorteventid)| {
			Ok(sauthevents
				.get(&shortstatekey)
				.map(move |(ty, sk)| ((ty, sk), shorteventid)))
		})
		.try_collect()
		.await?;
	let (state_keys, event_ids): (Vec<_>, Vec<_>) = matching_state.into_iter().unzip();

	self.services
		.short
		.multi_get_eventid_from_short(event_ids.into_iter().stream())
		.zip(state_keys.into_iter().stream())
		.map(|(event_id, state_key)| {
			event_id
				.map(|event_id| (state_key, event_id))
				.optional()
		})
		.ready_try_filter_map(Result::Ok)
		.broad_and_then(async |((ty, sk), event_id): ((&_, &_), OwnedEventId)| {
			self.services
				.timeline
				.get_pdu(&event_id)
				.map_ok(|pdu| ((ty.clone(), sk.clone()), pdu))
				.await
				.optional()
		})
		.ready_try_filter_map(Result::Ok)
		.try_collect()
		.await
}

/// Builds stripped invite-state context for a membership event.
///
/// Recommended room state cells are fetched on a best-effort basis, then the
/// supplied membership event is appended last. Failed state lookups are
/// omitted from the summary.
#[implement(Service)]
#[tracing::instrument(skip_all, level = "debug")]
pub async fn summary_stripped<Pdu: Event>(&self, event: &Pdu) -> Vec<Raw<AnyStrippedStateEvent>> {
	let cells = [
		(&StateEventType::RoomCreate, ""),
		(&StateEventType::RoomJoinRules, ""),
		(&StateEventType::RoomCanonicalAlias, ""),
		(&StateEventType::RoomName, ""),
		(&StateEventType::RoomAvatar, ""),
		(&StateEventType::RoomMember, event.sender().as_str()), // Add recommended events
		(&StateEventType::RoomEncryption, ""),
		(&StateEventType::RoomTopic, ""),
	];

	let fetches = cells.into_iter().map(|(event_type, state_key)| {
		self.services
			.state_accessor
			.room_state_get(event.room_id(), event_type, state_key)
	});

	join_all(fetches)
		.await
		.into_iter()
		.filter_map(Result::ok)
		.map(Event::into_format)
		.chain(once(event.to_format()))
		.collect()
}

/// Builds full-PDU invite-state context for a membership event.
///
/// Recommended stored state and the supplied event are formatted for the given
/// room version as required by MSC4311. Failed state or JSON lookups are
/// omitted, and the supplied membership event is appended last.
#[implement(Service)]
#[tracing::instrument(skip_all, level = "debug")]
pub async fn summary_pdus<Pdu: Event>(
	&self,
	event: &Pdu,
	event_json: &CanonicalJsonObject,
	room_version: &RoomVersionId,
) -> Vec<Box<RawJsonValue>> {
	let cells = [
		(&StateEventType::RoomCreate, ""),
		(&StateEventType::RoomJoinRules, ""),
		(&StateEventType::RoomCanonicalAlias, ""),
		(&StateEventType::RoomName, ""),
		(&StateEventType::RoomAvatar, ""),
		(&StateEventType::RoomMember, event.sender().as_str()),
		(&StateEventType::RoomEncryption, ""),
		(&StateEventType::RoomTopic, ""),
	];

	let membership = self
		.services
		.federation
		.format_pdu_into(event_json.clone(), Some(room_version))
		.boxed() // query-depth firewall
		.await;

	cells
		.into_iter()
		.stream()
		.wide_filter_map(async |(event_type, state_key)| {
			let pdu = self
				.services
				.state_accessor
				.room_state_get(event.room_id(), event_type, state_key)
				.await
				.ok()?;

			let pdu_json = self
				.services
				.timeline
				.get_pdu_json(pdu.event_id())
				.await
				.ok()?;

			Some(
				self.services
					.federation
					.format_pdu_into(pdu_json, Some(room_version))
					.await,
			)
		})
		.chain(once(membership).stream())
		.collect()
		.await
}

/// Returns the authorization and event-format rules for a room.
///
/// The rules are selected from the room version declared by its create event.
#[implement(Service)]
#[inline]
pub async fn get_room_version_rules(&self, room_id: &RoomId) -> Result<RoomVersionRules> {
	self.get_room_version(room_id)
		.await
		.and_then_ref(room_version::rules)
}

#[implement(Service)]
#[tracing::instrument(
	level = "debug"
	skip(self),
	ret(level = "trace"),
)]
/// Returns the room version declared by the room's create event.
///
/// Missing or malformed create-event content is reported to the caller.
pub async fn get_room_version(&self, room_id: &RoomId) -> Result<RoomVersionId> {
	self.services
		.state_accessor
		.room_state_get_content(room_id, &StateEventType::RoomCreate, "")
		.await
		.as_ref()
		.map(room_version::from_create_content)
		.cloned()
		.map_err(|e| err!(Request(NotFound("No create event found: {e:?}"))))
}

#[implement(Service)]
#[tracing::instrument(
	level = "debug"
	skip(self),
	ret(level = "trace"),
)]
/// Returns the short hash of a room's current state snapshot.
///
/// The lookup reads only the current room-to-state mapping and does not
/// reconstruct the snapshot.
pub async fn get_room_shortstatehash(&self, room_id: &RoomId) -> Result<ShortStateHash> {
	self.db
		.roomid_shortstatehash
		.get(room_id)
		.await
		.deserialized()
}

/// Returns the state hash recorded for an event.
///
/// The event ID is first resolved to its short event ID before the snapshot
/// association is read.
#[implement(Service)]
pub async fn pdu_shortstatehash(&self, event_id: &EventId) -> Result<ShortStateHash> {
	self.services
		.short
		.get_shorteventid(event_id)
		.and_then(|shorteventid| self.get_shortstatehash(shorteventid))
		.await
}

#[implement(Service)]
#[tracing::instrument(
	level = "debug"
	skip(self),
	ret(level = "trace"),
)]
/// Returns the state hash recorded for a short event ID.
///
/// This is the direct lookup used after an event ID has already been shortened.
pub async fn get_shortstatehash(&self, shorteventid: ShortEventId) -> Result<ShortStateHash> {
	const BUFSIZE: usize = size_of::<ShortEventId>();

	self.db
		.shorteventid_shortstatehash
		.aqry::<BUFSIZE, _>(&shorteventid)
		.await
		.deserialized()
}

/// Deletes a room's current state-hash mapping.
///
/// The supplied guard proves exclusive access to the room state mutation path;
/// compressed snapshots and event associations remain stored.
#[implement(Service)]
pub(super) fn delete_room_shortstatehash(
	&self,
	room_id: &RoomId,
	_mutex_lock: &Guard<OwnedRoomId, ()>,
) -> Result {
	self.db.roomid_shortstatehash.remove(room_id);

	Ok(())
}

/// Collapses a room to the resolvable forward extremity latest in stream order.
///
/// Rooms with at most one leaf, or with no leaf that resolves to a timeline
/// count, are left unchanged. The return value is the number of leaves removed.
#[implement(Service)]
#[tracing::instrument(
	level = "debug"
	skip_all,
	fields(%room_id),
)]
pub async fn collapse_forward_extremities(
	&self,
	room_id: &RoomId,
	state_lock: &RoomMutexGuard,
) -> usize {
	let extremities: ForwardExtremities = self
		.get_forward_extremities(room_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	if extremities.len() <= 1 {
		return 0;
	}

	let survivor = join_all(extremities.iter().map(async |event_id| {
		self.services
			.timeline
			.get_pdu_count(event_id)
			.await
			.ok()
			.map(|count| (count, event_id))
	}))
	.await
	.into_iter()
	.flatten()
	.max_by_key(|(count, _)| *count)
	.map(|(_, event_id)| event_id);

	let Some(survivor) = survivor else {
		return 0;
	};

	self.set_forward_extremities(room_id, once(&**survivor), state_lock)
		.await;

	extremities.len().saturating_sub(1)
}

#[implement(Service)]
#[tracing::instrument(
	level = "trace"
	skip(self),
)]
/// Streams the event IDs currently stored as a room's forward extremities.
///
/// Invalid rows and cursor errors are omitted. Returned references borrow the
/// database cursor and must be owned before they are retained across another
/// poll.
pub fn get_forward_extremities<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = &EventId> + Send + '_ {
	let prefix = (room_id, Interfix);

	self.db
		.roomid_pduleaves
		.keys_prefix(&prefix)
		.map_ok(|(_, event_id): (Ignore, &EventId)| event_id)
		.ignore_err()
}

#[implement(Service)]
#[tracing::instrument(
	level = "debug"
	skip_all,
	fields(%room_id),
)]
/// Replaces all stored forward extremities for a room.
///
/// Existing rows are removed before the supplied IDs are inserted while the
/// caller holds the state guard. The wipe and reinsertion are not transactional,
/// and errors encountered while scanning old rows are ignored.
pub async fn set_forward_extremities<'a, I>(
	&'a self,
	room_id: &'a RoomId,
	event_ids: I,
	_state_lock: &'a RoomMutexGuard,
) where
	I: Iterator<Item = &'a EventId> + Send + 'a,
{
	let prefix = (room_id, Interfix);
	self.db
		.roomid_pduleaves
		.keys_prefix_raw(&prefix)
		.ignore_err()
		.ready_for_each(|key| self.db.roomid_pduleaves.remove(key))
		.await;

	for event_id in event_ids {
		let key = (room_id, event_id);
		self.db.roomid_pduleaves.put_raw(key, event_id);
	}
}

/// Deletes every stored forward extremity for a room.
///
/// Cursor errors are ignored, so this best-effort cleanup always returns
/// success after removing every row it can read.
#[implement(Service)]
pub(super) async fn delete_all_rooms_forward_extremities(&self, room_id: &RoomId) -> Result {
	let prefix = (room_id, Interfix);

	self.db
		.roomid_pduleaves
		.keys_prefix_raw(&prefix)
		.ignore_err()
		.ready_for_each(|key| {
			trace!("Removing key: {key:?}");
			self.db.roomid_pduleaves.remove(key);
		})
		.await;

	Ok(())
}
