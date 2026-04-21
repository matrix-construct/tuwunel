mod append;
mod backfill;
mod build;
mod create;
mod pdus;
mod redact;

use std::{fmt::Write, sync::Arc};

use async_trait::async_trait;
use futures::{
	TryFutureExt, TryStreamExt,
	future::{
		Either::{Left, Right},
		select_ok,
	},
	pin_mut,
};
use ruma::{
	CanonicalJsonObject, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, RoomId,
	UserId, api::Direction, events::room::encrypted::Relation,
};
use serde::Deserialize;
pub use tuwunel_core::matrix::pdu::{PduId, RawPduId};
use tuwunel_core::{
	Err, Result, at, err, implement,
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
use tuwunel_database::{Database, Deserialized, Json, Map};

pub use self::pdus::PdusIterItem;
use crate::rooms::short::{ShortRoomId, ShortStateHash};

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	db: Data,
	pub mutex_insert: RoomMutexMap,
}

struct Data {
	eventid_outlierpdu: Arc<Map>,
	eventid_pduid: Arc<Map>,
	pduid_pdu: Arc<Map>,
	roomid_ts_pducount: Arc<Map>,
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

type RoomMutexMap = MutexMap<OwnedRoomId, ()>;
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
				roomid_ts_pducount: args.db["roomid_ts_pducount"].clone(),
				db: args.db.clone(),
			},
			mutex_insert: RoomMutexMap::new(),
		}))
	}

	async fn memory_usage(&self, out: &mut (dyn Write + Send)) -> Result {
		let mutex_insert = self.mutex_insert.len();
		writeln!(out, "insert_mutex: {mutex_insert}")?;

		Ok(())
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Removes a pdu and creates a new one with the same id.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn replace_pdu(&self, pdu_id: &RawPduId, pdu_json: &CanonicalJsonObject) -> Result {
	if self.db.pduid_pdu.get(pdu_id).await.is_not_found() {
		return Err!(Request(NotFound("PDU does not exist.")));
	}

	self.db.pduid_pdu.raw_put(pdu_id, Json(pdu_json));

	Ok(())
}

#[implement(Service)]
#[tracing::instrument(skip(self, pdu), level = "debug")]
pub fn add_pdu_outlier(&self, event_id: &EventId, pdu: &CanonicalJsonObject) {
	self.db
		.eventid_outlierpdu
		.raw_put(event_id, Json(pdu));
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn first_pdu_in_room(&self, room_id: &RoomId) -> Result<PduEvent> {
	self.first_item_in_room(room_id).await.map(at!(1))
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
#[inline]
pub async fn latest_pdu_in_room(&self, room_id: &RoomId) -> Result<PduEvent> {
	self.latest_item_in_room(None, room_id).await
}

#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn first_item_in_room(&self, room_id: &RoomId) -> Result<(PduCount, PduEvent)> {
	let pdus = self.pdus(None, room_id, None);

	pin_mut!(pdus);
	pdus.try_next()
		.await?
		.ok_or_else(|| err!(Request(NotFound("No PDU found in room"))))
}

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

/// Returns the shortstatehash of the room at the event directly preceding the
/// exclusive `before` param. `before` does not have to be a valid count
/// or in the room.
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

/// Returns the shortstatehash of the room at the event directly following the
/// exclusive `after` param. `after` does not have to be a valid count or
/// in the room.
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

/// Returns the shortstatehash of the room at the event
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

/// Returns the shorteventid in the room preceding the exclusive `before` param.
/// `before` does not have to be a valid shorteventid or in the room.
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

/// Returns the next shorteventid in the room after the exclusive `after` param.
/// `after` does not have to be a valid shorteventid or in the room.
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
	// +1 so we don't send the base event
	let pdu_id = PduId {
		shortroomid,
		count: count.saturating_inc(dir),
	};

	pdu_id.into()
}

/// Returns the pdu from shorteventid
/// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
#[implement(Service)]
pub async fn get_pdu_from_shorteventid(&self, shorteventid: ShortEventId) -> Result<PduEvent> {
	let event_id: OwnedEventId = self
		.services
		.short
		.get_eventid_from_short(shorteventid)
		.await?;

	self.get_pdu(&event_id).await
}

/// Returns the pdu.
/// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
#[implement(Service)]
pub async fn get_pdu(&self, event_id: &EventId) -> Result<PduEvent> { self.get(event_id).await }

/// Returns the pdu.
/// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
#[implement(Service)]
pub async fn get_outlier_pdu(&self, event_id: &EventId) -> Result<PduEvent> {
	self.get_outlier(event_id).await
}

/// Returns the pdu.
/// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
#[implement(Service)]
pub async fn get_non_outlier_pdu(&self, event_id: &EventId) -> Result<PduEvent> {
	self.get_non_outlier(event_id).await
}

/// Returns the pdu.
/// This does __NOT__ check the outliers `Tree`.
#[implement(Service)]
pub async fn get_pdu_from_id(&self, pdu_id: &RawPduId) -> Result<PduEvent> {
	self.get_from_id(pdu_id).await
}

/// Returns the json of a pdu.
/// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
#[implement(Service)]
pub async fn get_pdu_json(&self, event_id: &EventId) -> Result<CanonicalJsonObject> {
	self.get(event_id).await
}

/// Returns the json of a pdu.
/// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
#[implement(Service)]
pub async fn get_outlier_pdu_json(&self, event_id: &EventId) -> Result<CanonicalJsonObject> {
	self.get_outlier(event_id).await
}

/// Returns the json of a pdu.
/// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
#[implement(Service)]
pub async fn get_non_outlier_pdu_json(&self, event_id: &EventId) -> Result<CanonicalJsonObject> {
	self.get_non_outlier(event_id).await
}

/// Returns the pdu as a `BTreeMap<String, CanonicalJsonValue>`.
/// This does __NOT__ check the outliers `Tree`.
#[implement(Service)]
pub async fn get_pdu_json_from_id(&self, pdu_id: &RawPduId) -> Result<CanonicalJsonObject> {
	self.get_from_id(pdu_id).await
}

/// Returns the pdu into T.
/// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
#[implement(Service)]
#[inline]
pub async fn get<T>(&self, event_id: &EventId) -> Result<T>
where
	T: for<'de> Deserialize<'de>,
{
	let accepted = self.get_non_outlier(event_id);
	let outlier = self.get_outlier(event_id);

	pin_mut!(accepted, outlier);
	select_ok([Left(accepted), Right(outlier)])
		.await
		.map(at!(0))
}

/// Returns the pdu into T.
/// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
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

/// Returns the pdu into T.
/// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
#[implement(Service)]
#[inline]
pub async fn get_non_outlier<T>(&self, event_id: &EventId) -> Result<T>
where
	T: for<'de> Deserialize<'de>,
{
	let pdu_id = self.get_pdu_id(event_id).await?;

	self.get_from_id(&pdu_id).await
}

/// Returns the pdu into T.
/// This does __NOT__ check the outliers `Tree`.
#[implement(Service)]
#[inline]
pub async fn get_from_id<T>(&self, pdu_id: &RawPduId) -> Result<T>
where
	T: for<'de> Deserialize<'de>,
{
	self.db.pduid_pdu.get(pdu_id).await.deserialized()
}

/// Checks if pdu exists
/// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
#[implement(Service)]
pub async fn pdu_exists<'a>(&'a self, event_id: &'a EventId) -> bool {
	let non_outlier = self.non_outlier_pdu_exists(event_id);
	let outlier = self.outlier_pdu_exists(event_id);

	pin_mut!(non_outlier, outlier);
	select_ok([Left(non_outlier), Right(outlier)])
		.await
		.map(at!(0))
		.is_ok()
}

/// Like get_non_outlier_pdu(), but without the expense of fetching and
/// parsing the PduEvent
#[implement(Service)]
pub async fn non_outlier_pdu_exists(&self, event_id: &EventId) -> Result {
	let pduid = self.get_pdu_id(event_id).await?;

	self.db.pduid_pdu.exists(&pduid).await
}

/// Like get_non_outlier_pdu(), but without the expense of fetching and
/// parsing the PduEvent
#[implement(Service)]
#[inline]
pub async fn outlier_pdu_exists(&self, event_id: &EventId) -> Result {
	self.db.eventid_outlierpdu.exists(event_id).await
}

/// Returns the `count` of this pdu's id.
#[implement(Service)]
pub async fn get_pdu_count(&self, event_id: &EventId) -> Result<PduCount> {
	self.get_pdu_id(event_id)
		.await
		.map(RawPduId::pdu_count)
}

/// Returns the `shorteventid` from the `pdu_id`
#[implement(Service)]
pub async fn get_shorteventid_from_pdu_id(&self, pdu_id: &PduId) -> Result<ShortEventId> {
	let event_id = self.get_event_id_from_pdu_id(pdu_id).await?;

	self.services
		.short
		.get_shorteventid(&event_id)
		.await
}

/// Returns the `event_id` from the `pdu_id`
#[implement(Service)]
pub async fn get_event_id_from_pdu_id(&self, pdu_id: &PduId) -> Result<OwnedEventId> {
	let pdu_id: RawPduId = (*pdu_id).into();

	self.get_pdu_from_id(&pdu_id)
		.map_ok(|pdu| pdu.event_id)
		.await
}

/// Returns the `pdu_id` from the `shorteventid`
#[implement(Service)]
pub async fn get_pdu_id_from_shorteventid(&self, shorteventid: ShortEventId) -> Result<RawPduId> {
	let event_id: OwnedEventId = self
		.services
		.short
		.get_eventid_from_short(shorteventid)
		.await?;

	self.get_pdu_id(&event_id).await
}

/// Returns the pdu's id.
#[implement(Service)]
pub async fn get_pdu_id(&self, event_id: &EventId) -> Result<RawPduId> {
	self.db
		.eventid_pduid
		.get(event_id)
		.await
		.map(|handle| RawPduId::from(&*handle))
}
