//! Stores accepted room events, outliers, and their timeline indexes.
//!
//! The service maps event IDs to per-room stream positions, presents events to
//! users, and coordinates append, backfill, purge, and redaction operations.
//! Timeline insertion is serialized by the innermost per-room lock.

mod append;
mod backfill;
mod build;
mod create;
mod pdus;
mod purge;
mod redact;

#[cfg(test)]
mod tests;

use std::{fmt::Write, sync::Arc};

use async_trait::async_trait;
use futures::{
	FutureExt, TryFutureExt, TryStreamExt,
	future::{Either, select, select_ok},
	pin_mut,
};
use ruma::{
	CanonicalJsonObject, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedMxcUri,
	OwnedRoomId, RoomId, UserId, api::Direction, events::room::encrypted::Relation,
};
use serde::Deserialize;
/// Re-exports the typed and raw persistent timeline identifiers.
///
/// Both forms encode a room-local stream position suitable for database keys.
pub use tuwunel_core::matrix::pdu::{PduId, RawPduId};
use tuwunel_core::{
	Err, Error, Result, at, err, implement,
	matrix::{
		ShortEventId,
		pdu::{PduCount, PduEvent},
	},
	utils::{
		MutexMap, MutexMapGuard,
		result::{LogErr, NotFound},
		stream::TryReadyExt,
	},
	warn,
};
use tuwunel_database::{Database, Deserialized, Json, Map, Txn};

/// Re-exports the standard timeline item and count-key transformation.
///
/// Timeline consumers use these alongside the service's directional streams.
pub use self::pdus::{PdusIterItem, bias_count};
use crate::rooms::{
	short::{ShortRoomId, ShortStateHash},
	state_res::FetchEvent,
};

/// Provides persistent event lookup, insertion, and room timeline traversal.
///
/// Accepted events and outliers occupy separate maps while event IDs point to
/// accepted timeline positions. A per-room insertion lock serializes the final
/// mutation stage after federation and state work.
pub struct Service {
	services: Arc<crate::services::OnceServices>,
	db: Data,
	/// Serializes timeline insertion as the leaf per-room operation.
	///
	/// Acquire it after any federation or state mutex held for the same room.
	/// Never acquire either outer mutex while holding this guard.
	pub mutex_insert: RoomMutexMap,
}

struct Data {
	eventid_outlierpdu: Arc<Map>,
	eventid_pduid: Arc<Map>,
	pduid_pdu: Arc<Map>,
	roomid_tscount_pducount: Arc<Map>,
	db: Arc<Database>,
}

// Update Relationships
#[derive(Deserialize)]
struct ExtractRelatesTo {
	#[serde(rename = "m.relates_to")]
	relates_to: Relation,
}

#[derive(Clone, Debug, Deserialize)]
struct ExtractEventId {
	event_id: OwnedEventId,
}
#[derive(Clone, Debug, Deserialize)]
struct ExtractRelatesToEventId {
	#[serde(rename = "m.relates_to")]
	relates_to: ExtractEventId,
}

#[derive(Deserialize)]
struct ExtractBody {
	body: Option<String>,
}

#[derive(Deserialize)]
struct ExtractUrl {
	url: Option<OwnedMxcUri>,
}

type RoomMutexMap = MutexMap<OwnedRoomId, ()>;
/// Guard proving exclusive access to a room's timeline insertion path.
///
/// Acquire it after any federation or state guard held for the same room, and
/// never acquire either outer guard while retaining it.
pub type RoomMutexGuard = MutexMapGuard<OwnedRoomId, ()>;

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			db: Data {
				eventid_outlierpdu: args.db["eventid_outlierpdu"].clone(),
				eventid_pduid: args.db["eventid_pduid"].clone(),
				pduid_pdu: args.db["pduid_pdu"].clone(),
				roomid_tscount_pducount: args.db["roomid_tscount_pducount"].clone(),
				db: args.db.clone(),
			},
			mutex_insert: RoomMutexMap::new(),
		}))
	}

	async fn memory_usage(&self, out: &mut (dyn Write + Send)) -> Result {
		let mutex_insert = self.mutex_insert.len();
		writeln!(out, "- insert_mutex: {mutex_insert}")?;

		Ok(())
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Replaces the stored JSON of an accepted PDU without changing its ID.
///
/// A definite missing-row result is returned as `NotFound`; otherwise the
/// accepted timeline row is overwritten in place.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn replace_pdu(&self, pdu_id: &RawPduId, pdu_json: &CanonicalJsonObject) -> Result {
	if self.db.pduid_pdu.get(pdu_id).await.is_not_found() {
		return Err!(Request(NotFound("PDU does not exist.")));
	}

	self.db.pduid_pdu.raw_put(pdu_id, Json(pdu_json));

	Ok(())
}

/// Stage replacement of a PDU already loaded under its room's guard.
///
/// The caller retains the guard until the transaction commits.
#[implement(Service)]
pub(super) fn stage_replace_pdu(
	&self,
	txn: &mut Txn,
	pdu_id: &RawPduId,
	pdu_json: &CanonicalJsonObject,
) {
	txn.raw_put(&self.db.pduid_pdu, pdu_id, Json(pdu_json));
}

/// Stores an event as an outlier outside the accepted room timeline.
///
/// The event is keyed directly by event ID and no accepted-timeline mapping or
/// stream position is created.
#[implement(Service)]
#[tracing::instrument(skip(self, pdu), level = "debug")]
pub fn add_pdu_outlier(&self, event_id: &EventId, pdu: &CanonicalJsonObject) {
	self.db
		.eventid_outlierpdu
		.raw_put(event_id, Json(pdu));
}

/// Returns the earliest accepted PDU in a room.
///
/// Unknown or empty rooms report the underlying stream's not-found result.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn first_pdu_in_room(&self, room_id: &RoomId) -> Result<PduEvent> {
	self.first_item_in_room(room_id).await.map(at!(1))
}

/// Returns the latest accepted PDU in a room.
///
/// Presentation removes sender-only transaction metadata because no requesting
/// user is supplied.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
#[inline]
pub async fn latest_pdu_in_room(&self, room_id: &RoomId) -> Result<PduEvent> {
	self.latest_item_in_room(None, room_id).await
}

/// Returns the earliest accepted PDU and its room-local stream count.
///
/// The forward room stream provides the first available item after applying
/// ordinary presentation transformations.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn first_item_in_room(&self, room_id: &RoomId) -> Result<(PduCount, PduEvent)> {
	let pdus = self.pdus(None, room_id, None);

	pin_mut!(pdus);
	pdus.try_next()
		.await?
		.ok_or_else(|| err!(Request(NotFound("No PDU found in room"))))
}

/// Returns the latest accepted PDU in a room.
///
/// `sender_user` controls presentation of sender-only transaction metadata; it
/// does not filter events by sender.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn latest_item_in_room(
	&self,
	sender_user: Option<&UserId>,
	room_id: &RoomId,
) -> Result<PduEvent> {
	let pdus_rev = self.pdus_rev(sender_user, room_id, None);

	pin_mut!(pdus_rev);
	pdus_rev
		.try_next()
		.await?
		.map(at!(1))
		.ok_or_else(|| err!(Request(NotFound("No PDU's found in room"))))
}

/// Returns the state snapshot at the room event directly before a count.
///
/// The `before` boundary is exclusive and need not identify an existing event
/// or even belong to the room.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn prev_shortstatehash(
	&self,
	room_id: &RoomId,
	before: PduCount,
) -> Result<ShortStateHash> {
	let shortroomid: ShortRoomId = self
		.services
		.short
		.get_shortroomid(room_id)
		.await
		.map_err(|e| err!(Request(NotFound("Room {room_id:?} not found: {e:?}"))))?;

	let before = PduId { shortroomid, count: before };

	let prev = PduId {
		shortroomid,
		count: self.prev_timeline_count(&before).await?,
	};

	let shorteventid = self.get_shorteventid_from_pdu_id(&prev).await?;

	self.services
		.state
		.get_shortstatehash(shorteventid)
		.await
}

/// Returns the state snapshot at the room event directly after a count.
///
/// The `after` boundary is exclusive and need not identify an existing event
/// or even belong to the room.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn next_shortstatehash(
	&self,
	room_id: &RoomId,
	after: PduCount,
) -> Result<ShortStateHash> {
	let shortroomid: ShortRoomId = self
		.services
		.short
		.get_shortroomid(room_id)
		.await
		.map_err(|e| err!(Request(NotFound("Room {room_id:?} not found: {e:?}"))))?;

	let after = PduId { shortroomid, count: after };

	let next = PduId {
		shortroomid,
		count: self.next_timeline_count(&after).await?,
	};

	let shorteventid = self.get_shorteventid_from_pdu_id(&next).await?;

	self.services
		.state
		.get_shortstatehash(shorteventid)
		.await
}

/// Returns the state snapshot after every room event at or before a count.
///
/// The boundary is inclusive because a client's sync position is often the
/// count of the newest event it received, and need not belong to this room.
/// The snapshot precedes the first event strictly after the count, falling
/// back to current state only when no event follows; a count before the room's
/// first recorded snapshot is not found.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn shortstatehash_after(
	&self,
	room_id: &RoomId,
	count: PduCount,
) -> Result<ShortStateHash> {
	let shortroomid: ShortRoomId = self
		.services
		.short
		.get_shortroomid(room_id)
		.map_err(|e| err!(Request(NotFound("Room {room_id:?} not found: {e:?}"))))
		.await?;

	let after = PduId { shortroomid, count };
	let count = match self.next_timeline_count(&after).await {
		| Ok(count) => count,
		| Err(e) if !e.is_not_found() => return Err(e),
		| Err(_) => {
			return self
				.services
				.state
				.get_room_shortstatehash(room_id)
				.await;
		},
	};

	let next = PduId { shortroomid, count };
	let shorteventid = self.get_shorteventid_from_pdu_id(&next).await?;

	self.services
		.state
		.get_shortstatehash(shorteventid)
		.await
}

/// Returns the state snapshot recorded at a room timeline count.
///
/// The count is resolved through the room's accepted timeline row and then its
/// event-to-state association.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn get_shortstatehash(
	&self,
	room_id: &RoomId,
	count: PduCount,
) -> Result<ShortStateHash> {
	let shortroomid: ShortRoomId = self
		.services
		.short
		.get_shortroomid(room_id)
		.await
		.map_err(|e| err!(Request(NotFound("Room {room_id:?} not found: {e:?}"))))?;

	let pdu_id = PduId { shortroomid, count };

	let shorteventid = self.get_shorteventid_from_pdu_id(&pdu_id).await?;

	self.services
		.state
		.get_shortstatehash(shorteventid)
		.await
}

/// Returns the room timeline count directly before an encoded PDU ID.
///
/// The boundary is exclusive and need not identify an existing row. Its room
/// component selects the timeline prefix to scan.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn prev_timeline_count(&self, before: &PduId) -> Result<PduCount> {
	let before = Self::pdu_count_to_id(before.shortroomid, before.count, Direction::Backward);

	let pdu_ids = self
		.db
		.pduid_pdu
		.rev_keys_raw_from(&before)
		.ready_try_take_while(|pdu_id: &RawPduId| Ok(pdu_id.is_room_eq(before)))
		.ready_and_then(|pdu_id: RawPduId| Ok(pdu_id.pdu_count()));

	pin_mut!(pdu_ids);
	pdu_ids
		.try_next()
		.await
		.log_err()?
		.ok_or_else(|| err!(Request(NotFound("No earlier PDU's found in room"))))
}

/// Returns the room timeline count directly after an encoded PDU ID.
///
/// The boundary is exclusive and need not identify an existing row. Its room
/// component selects the timeline prefix to scan.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn next_timeline_count(&self, after: &PduId) -> Result<PduCount> {
	let after = Self::pdu_count_to_id(after.shortroomid, after.count, Direction::Forward);

	let pdu_ids = self
		.db
		.pduid_pdu
		.keys_raw_from(&after)
		.ready_try_take_while(|pdu_id: &RawPduId| Ok(pdu_id.is_room_eq(after)))
		.ready_and_then(|pdu_id: RawPduId| Ok(pdu_id.pdu_count()));

	pin_mut!(pdu_ids);
	pdu_ids
		.try_next()
		.await
		.log_err()?
		.ok_or(err!(Request(NotFound("No more PDU's found in room"))))
}

/// Returns the latest normal timeline count at or below an optional bound.
///
/// Backfilled counts are not returned. When no normal event qualifies, the
/// sentinel `PduCount::max()` is returned instead of a not-found error;
/// `sender_user` affects presentation only.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn last_timeline_count(
	&self,
	sender_user: Option<&UserId>,
	room_id: &RoomId,
	upper_bound: Option<PduCount>,
) -> Result<PduCount> {
	let upper_bound = upper_bound.unwrap_or_else(PduCount::max);
	let pdus_rev = self.pdus_rev(sender_user, room_id, None);

	pin_mut!(pdus_rev);
	let last_count = pdus_rev
		.ready_try_skip_while(|&(pducount, _)| Ok(pducount > upper_bound))
		.try_next()
		.await?
		.map(at!(0))
		.filter(|&count| matches!(count, PduCount::Normal(_)))
		.unwrap_or_else(PduCount::max);

	Ok(last_count)
}

/// Returns the indexed event ID nearest a timestamp in one direction.
///
/// Forward lookup selects the first event at or after the timestamp; backward
/// lookup selects the first event at or before it. The stored event timestamp
/// is returned with the ID.
#[implement(Service)]
pub async fn get_event_id_near_ts(
	&self,
	room_id: &RoomId,
	ts: MilliSecondsSinceUnixEpoch,
	dir: Direction,
) -> Result<(MilliSecondsSinceUnixEpoch, OwnedEventId)> {
	self.get_pdu_id_near_ts(room_id, ts, dir)
		.and_then(async |(ts, pdu_id)| {
			self.get_event_id_from_pdu_id(&pdu_id)
				.map_ok(|event_id| (ts, event_id))
				.await
		})
		.await
}

/// Returns the indexed PDU ID nearest a timestamp in one direction.
///
/// Forward lookup selects the first event at or after the timestamp; backward
/// lookup selects the first event at or before it. A room with no qualifying
/// event returns `NotFound`.
#[implement(Service)]
pub async fn get_pdu_id_near_ts(
	&self,
	room_id: &RoomId,
	ts: MilliSecondsSinceUnixEpoch,
	dir: Direction,
) -> Result<(MilliSecondsSinceUnixEpoch, PduId)> {
	let pdu_ids = self.pdu_ids_near_ts(room_id, ts, dir);

	pin_mut!(pdu_ids);
	pdu_ids
		.try_next()
		.await?
		.ok_or_else(|| err!(Request(NotFound("No event found near this timestamp."))))
}

/// Returns the accepted PDU nearest a timestamp in one direction.
///
/// The result contains the selected room-local count and decoded PDU. The
/// current `_user_id` parameter has no effect and no presentation or visibility
/// filtering is applied.
#[implement(Service)]
pub async fn get_pdu_near_ts(
	&self,
	_user_id: Option<&UserId>,
	room_id: &RoomId,
	ts: MilliSecondsSinceUnixEpoch,
	dir: Direction,
) -> Result<PdusIterItem> {
	let pdus = self
		.pdu_ids_near_ts(room_id, ts, dir)
		.map_ok(|(ts, pdu_id)| (ts, pdu_id.into()))
		.and_then(async |(_, pdu_id): (_, RawPduId)| {
			self.get_pdu_from_id(&pdu_id)
				.map_ok(|pdu| (pdu_id.pdu_count(), pdu))
				.await
		});

	pin_mut!(pdus);
	pdus.try_next()
		.await?
		.ok_or_else(|| err!(Request(NotFound("No event found near this timestamp."))))
}

#[implement(Service)]
async fn count_to_id(
	&self,
	room_id: &RoomId,
	count: PduCount,
	dir: Direction,
) -> Result<RawPduId> {
	let shortroomid: ShortRoomId = self
		.services
		.short
		.get_shortroomid(room_id)
		.await
		.map_err(|e| err!(Request(NotFound("Room {room_id:?} not found: {e:?}"))))?;

	Ok(Self::pdu_count_to_id(shortroomid, count, dir))
}

#[implement(Service)]
fn pdu_count_to_id(shortroomid: ShortRoomId, count: PduCount, dir: Direction) -> RawPduId {
	// In raw key order, backfilled zero precedes every stored row. It has no base
	// event, so retaining it keeps the `from` bound exclusive.
	let count = match (count, dir) {
		| (PduCount::Backfilled(0), Direction::Forward) => count,
		| _ => count.saturating_inc(dir),
	};

	let pdu_id = PduId { shortroomid, count };

	pdu_id.into()
}

/// Returns a decoded PDU resolved from a short event ID.
///
/// The short ID is expanded to an event ID before accepted and outlier storage
/// are queried through [`Service::get_pdu`].
#[implement(Service)]
pub async fn get_pdu_from_shorteventid(&self, shorteventid: ShortEventId) -> Result<PduEvent> {
	let event_id: OwnedEventId = self
		.services
		.short
		.get_eventid_from_short(shorteventid)
		.await?;

	self.get_pdu(&event_id).await
}

/// Returns a decoded PDU from accepted or outlier storage.
///
/// Both lookups are polled concurrently, so if duplicate rows exist the first
/// successful lookup determines the returned value.
#[implement(Service)]
pub async fn get_pdu(&self, event_id: &EventId) -> Result<PduEvent> { self.get(event_id).await }

/// Returns a decoded PDU from outlier storage.
///
/// Accepted timeline storage is not consulted, and storage or decoding errors
/// propagate to the caller.
#[implement(Service)]
pub async fn get_outlier_pdu(&self, event_id: &EventId) -> Result<PduEvent> {
	self.get_outlier(event_id).await
}

/// Returns a PDU from the accepted timeline.
///
/// Looks up the accepted record only, without consulting outliers. Storage and
/// decoding errors propagate to the caller.
#[implement(Service)]
pub async fn get_non_outlier_pdu(&self, event_id: &EventId) -> Result<PduEvent> {
	self.get_non_outlier(event_id).await
}

/// Returns a decoded PDU by its accepted timeline ID.
///
/// The accepted row is read directly without consulting the event-ID mapping
/// or outlier storage.
#[implement(Service)]
pub async fn get_pdu_from_id(&self, pdu_id: &RawPduId) -> Result<PduEvent> {
	self.get_from_id(pdu_id).await
}

/// Returns canonical PDU JSON from accepted or outlier storage.
///
/// Both lookups are polled concurrently, so if duplicate rows exist the first
/// successful lookup determines the returned value.
#[implement(Service)]
pub async fn get_pdu_json(&self, event_id: &EventId) -> Result<CanonicalJsonObject> {
	self.get(event_id).await
}

/// Returns canonical PDU JSON from outlier storage.
///
/// Accepted timeline storage is not consulted, and storage or decoding errors
/// propagate to the caller.
#[implement(Service)]
pub async fn get_outlier_pdu_json(&self, event_id: &EventId) -> Result<CanonicalJsonObject> {
	self.get_outlier(event_id).await
}

/// Returns the JSON of a PDU from the accepted timeline.
///
/// Looks up the accepted record only, without consulting outliers. Storage and
/// decoding errors propagate to the caller.
#[implement(Service)]
pub async fn get_non_outlier_pdu_json(&self, event_id: &EventId) -> Result<CanonicalJsonObject> {
	self.get_non_outlier(event_id).await
}

/// Returns canonical PDU JSON by its accepted timeline ID.
///
/// The accepted row is read directly without consulting the event-ID mapping
/// or outlier storage.
#[implement(Service)]
pub async fn get_pdu_json_from_id(&self, pdu_id: &RawPduId) -> Result<CanonicalJsonObject> {
	self.get_from_id(pdu_id).await
}

/// Deserializes an event from accepted or outlier storage into `T`.
///
/// Both lookups are polled concurrently, so if duplicate rows exist the first
/// successful lookup determines the returned value.
#[implement(Service)]
#[inline]
pub async fn get<T>(&self, event_id: &EventId) -> Result<T>
where
	T: for<'de> Deserialize<'de>,
{
	let accepted = self.get_non_outlier(event_id);
	let outlier = self.get_outlier(event_id);

	pin_mut!(accepted, outlier);
	select_ok([accepted.left_future(), outlier.right_future()])
		.await
		.map(at!(0))
}

impl FetchEvent for &Service {
	async fn get<T>(self, event_id: &EventId) -> Result<T>
	where
		T: for<'de> Deserialize<'de> + Send,
	{
		Service::get(self, event_id).await
	}

	async fn exists(self, event_id: &EventId) -> Result<bool> {
		let non_outlier = self.non_outlier_pdu_exists(event_id);
		let outlier = self.outlier_pdu_exists(event_id);
		let classify = |first: Error, second: Result| match second {
			| Ok(()) => Ok(true),
			| Err(second) if first.is_not_found() && second.is_not_found() => Ok(false),
			| Err(second) if first.is_not_found() => Err(second),
			| Err(_) => Err(first),
		};

		pin_mut!(non_outlier, outlier);
		match select(non_outlier, outlier).await {
			| Either::Left((Ok(()), _)) | Either::Right((Ok(()), _)) => Ok(true),
			| Either::Left((Err(first), second)) => classify(first, second.await),
			| Either::Right((Err(first), second)) => classify(first, second.await),
		}
	}
}

/// Deserializes an event from outlier storage into `T`.
///
/// Accepted timeline storage is not consulted, and storage or decoding errors
/// propagate to the caller.
#[implement(Service)]
#[inline]
pub async fn get_outlier<T>(&self, event_id: &EventId) -> Result<T>
where
	T: for<'de> Deserialize<'de>,
{
	self.db
		.eventid_outlierpdu
		.get(event_id)
		.await
		.deserialized()
}

/// Deserializes a PDU from the accepted timeline into `T`.
///
/// Resolves the event ID through `eventid_pduid` and reads `pduid_pdu`, without
/// consulting outliers. Storage and decoding errors propagate to the caller.
#[implement(Service)]
#[inline]
pub async fn get_non_outlier<T>(&self, event_id: &EventId) -> Result<T>
where
	T: for<'de> Deserialize<'de>,
{
	let pdu_id = self.get_pdu_id(event_id).await?;

	self.get_from_id(&pdu_id).await
}

/// Deserializes an accepted timeline row into `T` by PDU ID.
///
/// The row is read directly without consulting the event-ID mapping or outlier
/// storage.
#[implement(Service)]
#[inline]
pub async fn get_from_id<T>(&self, pdu_id: &RawPduId) -> Result<T>
where
	T: for<'de> Deserialize<'de>,
{
	self.db.pduid_pdu.get(pdu_id).await.deserialized()
}

/// Reports whether an event exists in accepted or outlier storage.
///
/// The two existence checks run concurrently, and any lookup errors are
/// treated as absence.
#[implement(Service)]
pub async fn pdu_exists<'a>(&'a self, event_id: &'a EventId) -> bool {
	let non_outlier = self.non_outlier_pdu_exists(event_id);
	let outlier = self.outlier_pdu_exists(event_id);

	pin_mut!(non_outlier, outlier);
	select_ok([non_outlier.left_future(), outlier.right_future()])
		.await
		.map(at!(0))
		.is_ok()
}

/// Returns a future that resolves on the next event-to-PDU mapping mutation.
///
/// Registration against the event-to-PDU mapping is eager, so the watcher is
/// installed before the returned future is first awaited. Accepted insertion
/// is the normal wakeup, but any mutation under the event-ID prefix can resolve
/// the future.
#[implement(Service)]
pub fn watch_event<'a>(&'a self, event_id: &EventId) -> impl Future<Output = ()> + Send + 'a {
	self.db
		.eventid_pduid
		.watch_raw_prefix_once(event_id)
}

/// Checks whether an event has an accepted timeline row.
///
/// The event-to-PDU mapping is resolved first and the target row is then tested
/// without fetching or decoding the PDU.
#[implement(Service)]
pub async fn non_outlier_pdu_exists(&self, event_id: &EventId) -> Result {
	let pduid = self.get_pdu_id(event_id).await?;

	self.db.pduid_pdu.exists(&pduid).await
}

/// Checks whether an event has an outlier row.
///
/// Accepted timeline storage is not consulted and the PDU is not fetched or
/// decoded.
#[implement(Service)]
#[inline]
pub async fn outlier_pdu_exists(&self, event_id: &EventId) -> Result {
	self.db.eventid_outlierpdu.exists(event_id).await
}

/// Returns the room-local timeline count assigned to an accepted event.
///
/// The event ID is resolved through the accepted event-to-PDU mapping.
#[implement(Service)]
pub async fn get_pdu_count(&self, event_id: &EventId) -> Result<PduCount> {
	self.get_pdu_id(event_id)
		.await
		.map(RawPduId::pdu_count)
}

/// Returns the short event ID represented by an accepted PDU ID.
///
/// The accepted row supplies the full event ID, which is then resolved through
/// the short-ID service.
#[implement(Service)]
pub async fn get_shorteventid_from_pdu_id(&self, pdu_id: &PduId) -> Result<ShortEventId> {
	let event_id = self.get_event_id_from_pdu_id(pdu_id).await?;

	self.services
		.short
		.get_shorteventid(&event_id)
		.await
}

/// Returns the event ID stored at an accepted PDU ID.
///
/// The accepted row is decoded as a PDU to recover its event ID.
#[implement(Service)]
pub async fn get_event_id_from_pdu_id(&self, pdu_id: &PduId) -> Result<OwnedEventId> {
	let pdu_id: RawPduId = (*pdu_id).into();

	self.get_pdu_from_id(&pdu_id)
		.map_ok(|pdu| pdu.event_id)
		.await
}

/// Returns the accepted PDU ID associated with a short event ID.
///
/// The short ID is first expanded to its full event ID before the timeline
/// mapping is read.
#[implement(Service)]
pub async fn get_pdu_id_from_shorteventid(&self, shorteventid: ShortEventId) -> Result<RawPduId> {
	let event_id: OwnedEventId = self
		.services
		.short
		.get_eventid_from_short(shorteventid)
		.await?;

	self.get_pdu_id(&event_id).await
}

/// Returns the accepted timeline ID associated with an event.
///
/// Outlier storage is not consulted because outliers have no room timeline
/// position.
#[implement(Service)]
pub async fn get_pdu_id(&self, event_id: &EventId) -> Result<RawPduId> {
	self.db
		.eventid_pduid
		.get(event_id)
		.await
		.map(|handle| RawPduId::from(&*handle))
}
