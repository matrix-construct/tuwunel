//! Compact numeric identifiers for room and event data.
//!
//! The service maps event IDs, state keys, state hashes, and room IDs into a shared global sequence
//! space. Event IDs and state keys have paired forward and reverse mappings. State hashes have a
//! partial forward index and no reverse mapping; room IDs have only a forward mapping.

use std::{borrow::Borrow, sync::Arc};

use futures::{FutureExt, Stream, StreamExt, pin_mut};
use ruma::{EventId, OwnedEventId, OwnedRoomId, RoomId, events::StateEventType};
use serde::Deserialize;
/// Numeric identifier types shared with Matrix data structures.
///
/// Aliases name the compact identifier contexts used by event, state-key, and room mappings.
pub use tuwunel_core::matrix::{ShortEventId, ShortId, ShortRoomId, ShortStateKey};
use tuwunel_core::{
	Err, Result, err, implement,
	matrix::StateKey,
	utils,
	utils::{
		IterStream, MutexMap,
		hash::sha256::Digest,
		stream::{ReadyExt, WidebandExt},
	},
};
use tuwunel_database::{Deserialized, Get, Map, Qry, Txn};

/// Allocates and resolves compact identifiers used by room storage.
///
/// Per-identity locks serialize first allocation, and database transactions keep new forward and
/// reverse mappings consistent where both are stored.
pub struct Service {
	db: Data,
	creating: Creating,
	services: Arc<crate::services::OnceServices>,
}

struct Data {
	eventid_shorteventid: Arc<Map>,
	shorteventid_eventid: Arc<Map>,
	statekey_shortstatekey: Arc<Map>,
	shortstatekey_statekey: Arc<Map>,
	roomid_shortroomid: Arc<Map>,
	statehash_shortstatehash: Arc<Map>,
}

/// Serializes concurrent allocations so one identity maps to one short id.
///
/// A guard is held across both the re-read that detects a competing
/// allocation and the writes that publish this one. Entries exist only while
/// a caller holds or awaits one, so an uncontended allocation leaves nothing
/// behind.
#[derive(Default)]
struct Creating {
	shorteventid: MutexMap<OwnedEventId, ()>,
	shortstatekey: MutexMap<(StateEventType, StateKey), ()>,
	shortstatehash: MutexMap<Digest, ()>,
	shortroomid: MutexMap<OwnedRoomId, ()>,
}

/// Compact identifier assigned to a complete room state hash.
///
/// State-hash IDs share the global numeric sequence with the other short identifier families.
pub type ShortStateHash = ShortId;

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				eventid_shorteventid: args.db["eventid_shorteventid"].clone(),
				shorteventid_eventid: args.db["shorteventid_eventid"].clone(),
				statekey_shortstatekey: args.db["statekey_shortstatekey"].clone(),
				shortstatekey_statekey: args.db["shortstatekey_statekey"].clone(),
				roomid_shortroomid: args.db["roomid_shortroomid"].clone(),
				statehash_shortstatehash: args.db["statehash_shortstatehash"].clone(),
			},
			creating: Creating::default(),
			services: args.services.clone(),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Resolves an event ID to its compact identifier, allocating after an unsuccessful lookup.
///
/// New allocations publish the forward and reverse mappings in one transaction. Concurrent callers
/// for the same event converge on one identifier.
///
/// # Panics
///
/// Panics when identifier dispatch or the paired mapping transaction fails.
#[implement(Service)]
pub async fn get_or_create_shorteventid(&self, event_id: &EventId) -> ShortEventId {
	if let Ok(shorteventid) = self.get_shorteventid(event_id).await {
		return shorteventid;
	}

	self.create_shorteventid(event_id).await
}

/// Resolves each event ID to its short ID, allocating after unsuccessful lookups.
///
/// Allocation runs ahead of consumer demand, so a caller that stops early
/// still allocates for the events already buffered. Today's callers drain the
/// stream in full.
///
/// # Panics
///
/// Panics when an existing mapping is malformed, identifier dispatch fails, or a paired mapping
/// transaction fails.
#[implement(Service)]
pub fn multi_get_or_create_shorteventid<'a, I>(
	&'a self,
	event_ids: I,
) -> impl Stream<Item = ShortEventId> + Send + '_
where
	I: Iterator<Item = &'a EventId> + Clone + Send + 'a,
{
	event_ids
		.clone()
		.stream()
		.get(&self.db.eventid_shorteventid)
		.zip(event_ids.into_iter().stream())
		.wide_then(async |(result, event_id)| match result {
			| Ok(ref short) => utils::u64_from_u8(short),
			| Err(_) => self.create_shorteventid(event_id).await,
		})
}

#[implement(Service)]
async fn create_shorteventid(&self, event_id: &EventId) -> ShortEventId {
	let _lock = self.creating.shorteventid.lock(event_id).await;

	if let Ok(shorteventid) = self.get_shorteventid(event_id).await {
		return shorteventid;
	}

	let short = self.services.globals.next_count();
	let mut txn = self.services.db.txn();

	txn.insert_raw(&self.db.shorteventid_eventid, (*short).to_be_bytes(), event_id);
	txn.insert_raw(&self.db.eventid_shorteventid, event_id, (*short).to_be_bytes());
	txn.execute();

	*short
}

/// Resolves an event ID to its existing compact identifier.
///
/// Missing or malformed mappings are returned as database errors.
#[implement(Service)]
pub async fn get_shorteventid(&self, event_id: &EventId) -> Result<ShortEventId> {
	self.db
		.eventid_shorteventid
		.get(event_id)
		.await
		.deserialized()
}

/// Resolves a state event type and key, allocating an identifier after an unsuccessful lookup.
///
/// New allocations publish the forward and reverse mappings in one transaction. Concurrent callers
/// for the same pair converge on one identifier.
///
/// # Panics
///
/// Panics when identifier dispatch or the paired mapping transaction fails.
#[implement(Service)]
pub async fn get_or_create_shortstatekey(
	&self,
	event_type: &StateEventType,
	state_key: &str,
) -> ShortStateKey {
	if let Ok(shortstatekey) = self
		.get_shortstatekey(event_type, state_key)
		.await
	{
		return shortstatekey;
	}

	self.create_shortstatekey(event_type, state_key)
		.await
}

#[implement(Service)]
async fn create_shortstatekey(
	&self,
	event_type: &StateEventType,
	state_key: &str,
) -> ShortStateKey {
	let owned_key = (event_type.clone(), StateKey::from_str(state_key));
	let _lock = self.creating.shortstatekey.lock(&owned_key).await;

	if let Ok(shortstatekey) = self
		.get_shortstatekey(event_type, state_key)
		.await
	{
		return shortstatekey;
	}

	let key = (event_type, state_key);
	let shortstatekey = self.services.globals.next_count();
	let mut txn = self.services.db.txn();

	txn.put(&self.db.shortstatekey_statekey, *shortstatekey, key);
	txn.put(&self.db.statekey_shortstatekey, key, *shortstatekey);
	txn.execute();

	*shortstatekey
}

/// Resolves a state event type and key to its existing compact identifier.
///
/// The event type and state key together form the forward lookup key. Missing or malformed mappings
/// are returned as database errors.
#[implement(Service)]
pub async fn get_shortstatekey(
	&self,
	event_type: &StateEventType,
	state_key: &str,
) -> Result<ShortStateKey> {
	let key = (event_type, state_key);
	self.db
		.statekey_shortstatekey
		.qry(&key)
		.await
		.deserialized()
}

/// Resolves a compact event identifier through the reverse mapping.
///
/// The caller selects a deserializable event-ID representation whose owned form can be borrowed as
/// [`EventId`]. Missing or malformed rows are reported with the compact identifier.
#[implement(Service)]
pub async fn get_eventid_from_short<Id>(&self, shorteventid: ShortEventId) -> Result<Id>
where
	Id: for<'de> Deserialize<'de> + Send + Sized + ToOwned,
	<Id as ToOwned>::Owned: Borrow<EventId>,
{
	const BUFSIZE: usize = size_of::<ShortEventId>();

	self.db
		.shorteventid_eventid
		.aqry::<BUFSIZE, _>(&shorteventid)
		.await
		.deserialized()
		.map_err(|e| err!(Database("Failed to find EventId from short {shorteventid:?}: {e:?}")))
}

/// Resolves a stream of compact event identifiers through the reverse mapping.
///
/// Input order is preserved. Each output carries its own lookup or deserialization result.
#[implement(Service)]
pub fn multi_get_eventid_from_short<'a, Id, S>(
	&'a self,
	shorteventid: S,
) -> impl Stream<Item = Result<Id>> + Send + 'a
where
	S: Stream<Item = ShortEventId> + Send + 'a,
	Id: for<'de> Deserialize<'de> + Send + Sized + ToOwned + 'a,
	<Id as ToOwned>::Owned: Borrow<EventId>,
{
	shorteventid
		.qry(&self.db.shorteventid_eventid)
		.map(Deserialized::deserialized)
}

/// Resolves a compact state-key identifier to its event type and state key.
///
/// Missing or malformed reverse mappings are returned as database errors that identify the compact
/// value.
#[implement(Service)]
pub async fn get_statekey_from_short(
	&self,
	shortstatekey: ShortStateKey,
) -> Result<(StateEventType, StateKey)> {
	const BUFSIZE: usize = size_of::<ShortStateKey>();

	self.db
		.shortstatekey_statekey
		.aqry::<BUFSIZE, _>(&shortstatekey)
		.await
		.deserialized()
		.map_err(|e| {
			err!(Database(
				"Failed to find (StateEventType, state_key) from short {shortstatekey:?}: {e:?}"
			))
		})
}

/// Resolves a stream of compact state-key identifiers through the reverse mapping.
///
/// Input order is preserved. Each output carries its own lookup or deserialization result.
#[implement(Service)]
pub fn multi_get_statekey_from_short<'a, S>(
	&'a self,
	shortstatekey: S,
) -> impl Stream<Item = Result<(StateEventType, StateKey)>> + Send + 'a
where
	S: Stream<Item = ShortStateKey> + Send + 'a,
{
	shortstatekey
		.qry(&self.db.shortstatekey_statekey)
		.map(Deserialized::deserialized)
}

/// Resolves a state hash, allocating a compact identifier after an unsuccessful lookup.
///
/// The returned boolean is `true` when the mapping already existed, in which case the callback is
/// skipped. For a new identifier, `write_statediff` stages its associated state diff in the same
/// transaction as the mapping.
///
/// # Errors
///
/// Returns an error when `write_statediff` rejects the new state diff. Neither row is committed.
///
/// # Panics
///
/// Panics when identifier dispatch or the mapping and state-diff transaction fails.
#[implement(Service)]
pub async fn get_or_create_shortstatehash<F>(
	&self,
	state_hash: &Digest,
	write_statediff: F,
) -> Result<(ShortStateHash, bool)>
where
	F: FnOnce(&mut Txn, ShortStateHash) -> Result,
{
	if let Ok(shortstatehash) = self.get_shortstatehash(state_hash).await {
		return Ok((shortstatehash, true));
	}

	self.create_shortstatehash(state_hash, write_statediff)
		.await
}

#[implement(Service)]
async fn create_shortstatehash<F>(
	&self,
	state_hash: &Digest,
	write_statediff: F,
) -> Result<(ShortStateHash, bool)>
where
	F: FnOnce(&mut Txn, ShortStateHash) -> Result,
{
	let _lock = self
		.creating
		.shortstatehash
		.lock(state_hash)
		.await;

	if let Ok(shortstatehash) = self.get_shortstatehash(state_hash).await {
		return Ok((shortstatehash, true));
	}

	let shortstatehash = self.services.globals.next_count();
	let mut txn = self.services.db.txn();

	txn.insert_raw(
		&self.db.statehash_shortstatehash,
		state_hash,
		(*shortstatehash).to_be_bytes(),
	);
	write_statediff(&mut txn, *shortstatehash)?;
	txn.execute();

	Ok((*shortstatehash, false))
}

/// Resolves a state hash to its existing compact identifier.
///
/// Missing or malformed mappings are returned as database errors.
#[implement(Service)]
pub async fn get_shortstatehash(&self, state_hash: &Digest) -> Result<ShortStateHash> {
	self.db
		.statehash_shortstatehash
		.get(state_hash)
		.await
		.deserialized()
}

/// Resolves a room ID to its existing compact identifier.
///
/// Missing or malformed mappings are returned as database errors.
#[implement(Service)]
pub async fn get_shortroomid(&self, room_id: &RoomId) -> Result<ShortRoomId> {
	self.db
		.roomid_shortroomid
		.get(room_id)
		.await
		.deserialized()
}

/// Resolves a compact room identifier by scanning the forward mapping.
///
/// Room identifiers have no dedicated reverse map, so the first matching value is returned.
/// Unreadable rows are skipped during the scan.
#[implement(Service)]
pub async fn get_roomid_from_short(&self, shortroomid_: ShortRoomId) -> Result<OwnedRoomId> {
	let stream = self
		.db
		.roomid_shortroomid
		.stream()
		.ready_filter_map(Result::ok);

	pin_mut!(stream);
	stream
		.ready_find(|&(_, shortroomid)| shortroomid == shortroomid_)
		.map(|found| found.map(|(room_id, _): (&RoomId, ShortRoomId)| room_id.to_owned()))
		.await
		.ok_or_else(|| err!(Database("Failed to find RoomId from {shortroomid_:?}")))
}

/// Resolves a room ID to its compact identifier, allocating after an unsuccessful lookup.
///
/// Room mappings are stored only in the forward direction. Concurrent callers for the same room
/// converge on one identifier.
///
/// # Panics
///
/// Panics when identifier dispatch or storing the forward mapping fails.
#[implement(Service)]
pub async fn get_or_create_shortroomid(&self, room_id: &RoomId) -> ShortRoomId {
	if let Ok(shortroomid) = self.get_shortroomid(room_id).await {
		return shortroomid;
	}

	self.create_shortroomid(room_id).await
}

#[implement(Service)]
async fn create_shortroomid(&self, room_id: &RoomId) -> ShortRoomId {
	const BUFSIZE: usize = size_of::<ShortRoomId>();

	let _lock = self.creating.shortroomid.lock(room_id).await;

	if let Ok(shortroomid) = self.get_shortroomid(room_id).await {
		return shortroomid;
	}

	let short = self.services.globals.next_count();

	debug_assert!(size_of_val(&*short) == BUFSIZE, "buffer requirement changed");

	self.db
		.roomid_shortroomid
		.raw_aput::<BUFSIZE, _, _>(room_id, *short);

	*short
}

/// Deletes the compact identifier mapping for a room.
///
/// Only the forward room mapping is removed. Any unsuccessful existence check is returned as a
/// not-found database error.
#[implement(Service)]
pub async fn delete_shortroomid(&self, room_id: &RoomId) -> Result {
	if self
		.db
		.roomid_shortroomid
		.exists(room_id)
		.await
		.is_ok()
	{
		self.db.roomid_shortroomid.remove(room_id);
		Ok(())
	} else {
		Err!(Database("not found"))
	}
}
