mod append;
mod backfill;
mod build;
mod create;
mod redact;

use std::{borrow::Borrow, fmt::Write, sync::Arc};

use async_trait::async_trait;
use futures::{
	Stream, TryFutureExt, TryStreamExt,
	future::{
		Either::{Left, Right},
		select_ok,
	},
	pin_mut,
};
use ruma::{
	CanonicalJsonObject, EventId, OwnedEventId, OwnedRoomId, RoomId, UserId, api::Direction,
	events::room::encrypted::Relation,
};
use serde::Deserialize;
pub use tuwunel_core::matrix::pdu::{PduId, RawPduId};
use tuwunel_core::{
	Err, Result, Server, at, err, implement,
	matrix::pdu::{PduCount, PduEvent},
	utils::{
		MutexMap, MutexMapGuard,
		result::{LogErr, NotFound},
		stream::{TryIgnore, TryReadyExt},
	},
	warn,
};
use tuwunel_database::{Database, Deserialized, Json, KeyVal, Map};

use crate::{
	Dep, account_data, admin, appservice, globals, pusher, rooms, rooms::short::ShortRoomId,
	sending, server_keys, users,
};

pub struct Service {
	services: Services,
	db: Data,
	pub mutex_insert: RoomMutexMap,
}

struct Services {
	server: Arc<Server>,
	account_data: Dep<account_data::Service>,
	appservice: Dep<appservice::Service>,
	admin: Dep<admin::Service>,
	alias: Dep<rooms::alias::Service>,
	globals: Dep<globals::Service>,
	short: Dep<rooms::short::Service>,
	state: Dep<rooms::state::Service>,
	state_cache: Dep<rooms::state_cache::Service>,
	state_accessor: Dep<rooms::state_accessor::Service>,
	pdu_metadata: Dep<rooms::pdu_metadata::Service>,
	read_receipt: Dep<rooms::read_receipt::Service>,
	sending: Dep<sending::Service>,
	server_keys: Dep<server_keys::Service>,
	user: Dep<rooms::user::Service>,
	users: Dep<users::Service>,
	pusher: Dep<pusher::Service>,
	threads: Dep<rooms::threads::Service>,
	search: Dep<rooms::search::Service>,
	spaces: Dep<rooms::spaces::Service>,
	event_handler: Dep<rooms::event_handler::Service>,
}

struct Data {
	eventid_outlierpdu: Arc<Map>,
	eventid_pduid: Arc<Map>,
	pduid_pdu: Arc<Map>,
	userroomid_highlightcount: Arc<Map>,
	userroomid_notificationcount: Arc<Map>,
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
pub type PdusIterItem = (PduCount, PduEvent);

#[async_trait]
impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: Services {
				server: args.server.clone(),
				account_data: args.depend::<account_data::Service>("account_data"),
				appservice: args.depend::<appservice::Service>("appservice"),
				admin: args.depend::<admin::Service>("admin"),
				alias: args.depend::<rooms::alias::Service>("rooms::alias"),
				globals: args.depend::<globals::Service>("globals"),
				short: args.depend::<rooms::short::Service>("rooms::short"),
				state: args.depend::<rooms::state::Service>("rooms::state"),
				state_cache: args.depend::<rooms::state_cache::Service>("rooms::state_cache"),
				state_accessor: args
					.depend::<rooms::state_accessor::Service>("rooms::state_accessor"),
				pdu_metadata: args.depend::<rooms::pdu_metadata::Service>("rooms::pdu_metadata"),
				read_receipt: args.depend::<rooms::read_receipt::Service>("rooms::read_receipt"),
				sending: args.depend::<sending::Service>("sending"),
				server_keys: args.depend::<server_keys::Service>("server_keys"),
				user: args.depend::<rooms::user::Service>("rooms::user"),
				users: args.depend::<users::Service>("users"),
				pusher: args.depend::<pusher::Service>("pusher"),
				threads: args.depend::<rooms::threads::Service>("rooms::threads"),
				search: args.depend::<rooms::search::Service>("rooms::search"),
				spaces: args.depend::<rooms::spaces::Service>("rooms::spaces"),
				event_handler: args
					.depend::<rooms::event_handler::Service>("rooms::event_handler"),
			},
			db: Data {
				eventid_outlierpdu: args.db["eventid_outlierpdu"].clone(),
				eventid_pduid: args.db["eventid_pduid"].clone(),
				pduid_pdu: args.db["pduid_pdu"].clone(),
				userroomid_highlightcount: args.db["userroomid_highlightcount"].clone(),
				userroomid_notificationcount: args.db["userroomid_notificationcount"].clone(),
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
		.ok_or_else(|| err!(Request(NotFound("no PDU's found in room"))))
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

/// Returns an iterator over all PDUs in a room. Unknown rooms produce no
/// items.
#[implement(Service)]
#[inline]
pub fn all_pdus<'a>(
	&'a self,
	user_id: &'a UserId,
	room_id: &'a RoomId,
) -> impl Stream<Item = PdusIterItem> + Send + 'a {
	self.pdus(Some(user_id), room_id, None)
		.ignore_err()
}

/// Returns an iterator over all events and their tokens in a room that
/// happened after the event with id `from` in order.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn pdus<'a>(
	&'a self,
	user_id: Option<&'a UserId>,
	room_id: &'a RoomId,
	from: Option<PduCount>,
) -> impl Stream<Item = Result<PdusIterItem>> + Send + 'a {
	let from = from.unwrap_or_else(PduCount::min);
	self.count_to_id(room_id, from, Direction::Forward)
		.map_ok(move |current| {
			let prefix = current.shortroomid();
			self.db
				.pduid_pdu
				.raw_stream_from(&current)
				.ready_try_take_while(move |(key, _)| Ok(key.starts_with(&prefix)))
				.ready_and_then(move |item| Self::each_pdu(item, user_id))
		})
		.try_flatten_stream()
}

/// Returns an iterator over all events and their tokens in a room that
/// happened before the event with id `until` in reverse-order.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn pdus_rev<'a>(
	&'a self,
	user_id: Option<&'a UserId>,
	room_id: &'a RoomId,
	until: Option<PduCount>,
) -> impl Stream<Item = Result<PdusIterItem>> + Send + 'a {
	let until = until.unwrap_or_else(PduCount::max);
	self.count_to_id(room_id, until, Direction::Backward)
		.map_ok(move |current| {
			let prefix = current.shortroomid();
			self.db
				.pduid_pdu
				.rev_raw_stream_from(&current)
				.ready_try_take_while(move |(key, _)| Ok(key.starts_with(&prefix)))
				.ready_and_then(move |item| Self::each_pdu(item, user_id))
		})
		.try_flatten_stream()
}

#[implement(Service)]
fn each_pdu((pdu_id, pdu): KeyVal<'_>, user_id: Option<&UserId>) -> Result<PdusIterItem> {
	let pdu_id: RawPduId = pdu_id.into();
	let mut pdu = serde_json::from_slice::<PduEvent>(pdu)?;

	if Some(pdu.sender.borrow()) != user_id {
		pdu.remove_transaction_id().log_err().ok();
	}

	pdu.add_age().log_err().ok();

	Ok((pdu_id.pdu_count(), pdu))
}

#[implement(Service)]
async fn count_to_id(
	&self,
	room_id: &RoomId,
	shorteventid: PduCount,
	dir: Direction,
) -> Result<RawPduId> {
	let shortroomid: ShortRoomId = self
		.services
		.short
		.get_shortroomid(room_id)
		.await
		.map_err(|e| err!(Request(NotFound("Room {room_id:?} not found: {e:?}"))))?;

	// +1 so we don't send the base event
	let pdu_id = PduId {
		shortroomid,
		shorteventid: shorteventid.saturating_inc(dir),
	};

	Ok(pdu_id.into())
}

/// Returns the pdu.
///
/// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
#[implement(Service)]
pub async fn get_pdu(&self, event_id: &EventId) -> Result<PduEvent> {
	let accepted = self.get_non_outlier_pdu(event_id);
	let outlier = self.get_outlier_pdu(event_id);

	pin_mut!(accepted, outlier);
	select_ok([Left(accepted), Right(outlier)])
		.await
		.map(at!(0))
}

/// Returns the pdu.
///
/// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
#[implement(Service)]
pub async fn get_outlier_pdu(&self, event_id: &EventId) -> Result<PduEvent> {
	self.db
		.eventid_outlierpdu
		.get(event_id)
		.await
		.deserialized()
}

/// Returns the pdu.
///
/// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
#[implement(Service)]
pub async fn get_non_outlier_pdu(&self, event_id: &EventId) -> Result<PduEvent> {
	let pdu_id = self.get_pdu_id(event_id).await?;

	self.get_pdu_from_id(&pdu_id).await
}

/// Returns the pdu.
///
/// This does __NOT__ check the outliers `Tree`.
#[implement(Service)]
pub async fn get_pdu_from_id(&self, pdu_id: &RawPduId) -> Result<PduEvent> {
	self.db.pduid_pdu.get(pdu_id).await.deserialized()
}

/// Returns the json of a pdu.
#[implement(Service)]
pub async fn get_pdu_json(&self, event_id: &EventId) -> Result<CanonicalJsonObject> {
	let accepted = self.get_non_outlier_pdu_json(event_id);
	let outlier = self.get_outlier_pdu_json(event_id);

	pin_mut!(accepted, outlier);
	select_ok([Left(accepted), Right(outlier)])
		.await
		.map(at!(0))
}

/// Returns the json of a pdu.
#[implement(Service)]
pub async fn get_outlier_pdu_json(&self, event_id: &EventId) -> Result<CanonicalJsonObject> {
	self.db
		.eventid_outlierpdu
		.get(event_id)
		.await
		.deserialized()
}

/// Returns the json of a pdu.
#[implement(Service)]
pub async fn get_non_outlier_pdu_json(&self, event_id: &EventId) -> Result<CanonicalJsonObject> {
	let pdu_id = self.get_pdu_id(event_id).await?;

	self.get_pdu_json_from_id(&pdu_id).await
}

/// Returns the pdu as a `BTreeMap<String, CanonicalJsonValue>`.
#[implement(Service)]
pub async fn get_pdu_json_from_id(&self, pdu_id: &RawPduId) -> Result<CanonicalJsonObject> {
	self.db.pduid_pdu.get(pdu_id).await.deserialized()
}

/// Checks if pdu exists
///
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
		.map(|pdu_id| pdu_id.pdu_count())
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
