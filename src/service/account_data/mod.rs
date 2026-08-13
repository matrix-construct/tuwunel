mod direct;
mod push_rules;
mod room_tags;

use std::sync::Arc;

use futures::{Stream, StreamExt, TryFutureExt, pin_mut};
use ruma::{
	RoomId, UserId,
	events::{
		AnyGlobalAccountDataEvent, AnyRawAccountDataEvent, AnyRoomAccountDataEvent,
		GlobalAccountDataEventType, RoomAccountDataEventType,
	},
	serde::Raw,
};
use serde::Deserialize;
use serde_json::json;
use tuwunel_core::{
	Err, Result, at, err, implement,
	utils::{ReadyExt, TryReadyExt, result::LogErr, stream::TryIgnore},
};
use tuwunel_database::{Deserialized, Handle, Ignore, Interfix, Json, Map};

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	db: Data,
}

struct Data {
	roomuserdataid_accountdata: Arc<Map>,
	roomusertype_roomuserdataid: Arc<Map>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			db: Data {
				roomuserdataid_accountdata: args.db["roomuserdataid_accountdata"].clone(),
				roomusertype_roomuserdataid: args.db["roomusertype_roomuserdataid"].clone(),
			},
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Places one event in the account data of the user and removes the
/// previous entry.
#[implement(Service)]
pub async fn update(
	&self,
	room_id: Option<&RoomId>,
	user_id: &UserId,
	event_type: RoomAccountDataEventType,
	data: &serde_json::Value,
) -> Result {
	if data.get("type").is_none() || data.get("content").is_none() {
		return Err!(Request(InvalidParam("Account data doesn't have all required fields.")));
	}

	let count = self.services.globals.next_count();
	let roomuserdataid = (room_id, user_id, *count, &event_type);
	let key = (room_id, user_id, &event_type);
	let prev = self
		.db
		.roomusertype_roomuserdataid
		.qry(&key)
		.await;

	let mut txn = self.services.db.txn();

	txn.put(&self.db.roomuserdataid_accountdata, roomuserdataid, Json(data));
	txn.put(&self.db.roomusertype_roomuserdataid, key, roomuserdataid);

	if let Ok(prev) = prev {
		txn.del_raw(&self.db.roomuserdataid_accountdata, prev);
	}

	txn.execute();

	Ok(())
}

/// MSC3391: replace the stored event with a tombstone whose content is
/// `{}`. Delta sync surfaces the empty content so clients can apply the
/// deletion; initial sync and GET treat the tombstone as not-present.
#[implement(Service)]
pub async fn delete(
	&self,
	room_id: Option<&RoomId>,
	user_id: &UserId,
	event_type: RoomAccountDataEventType,
) -> Result {
	let tombstone = json!({
		"type": event_type.to_string(),
		"content": {},
	});

	self.update(room_id, user_id, event_type, &tombstone)
		.await
}

/// Searches the room account data for a specific kind.
#[implement(Service)]
pub async fn get_global<T>(&self, user_id: &UserId, kind: GlobalAccountDataEventType) -> Result<T>
where
	T: for<'de> Deserialize<'de>,
{
	self.get_raw(None, user_id, &kind.to_string())
		.await
		.deserialized()
}

/// Searches the global account data for a specific kind.
#[implement(Service)]
pub async fn get_room<T>(
	&self,
	room_id: &RoomId,
	user_id: &UserId,
	kind: RoomAccountDataEventType,
) -> Result<T>
where
	T: for<'de> Deserialize<'de>,
{
	self.get_raw(Some(room_id), user_id, &kind.to_string())
		.await
		.deserialized()
}

#[implement(Service)]
pub async fn get_raw(
	&self,
	room_id: Option<&RoomId>,
	user_id: &UserId,
	kind: &str,
) -> Result<Handle<'_>> {
	let key = (room_id, user_id, kind.to_owned());
	self.db
		.roomusertype_roomuserdataid
		.qry(&key)
		.and_then(|roomuserdataid| {
			self.db
				.roomuserdataid_accountdata
				.get(&roomuserdataid)
		})
		.await
}

/// Returns all changes to the account data that happened after `since`.
#[implement(Service)]
pub fn changes_since<'a>(
	&'a self,
	room_id: Option<&'a RoomId>,
	user_id: &'a UserId,
	since: u64,
	to: Option<u64>,
) -> impl Stream<Item = AnyRawAccountDataEvent> + Send + 'a {
	self.changes_since_fallible(room_id, user_id, since, to)
		.map(LogErr::log_err)
		.ignore_err()
}

/// Returns bounded account-data changes without suppressing failures.
///
/// The lower bound is exclusive and the optional upper bound is inclusive.
/// Cursor, decode, and deserialization failures remain in the stream for an
/// atomic caller to handle.
#[implement(Service)]
pub fn changes_since_fallible<'a>(
	&'a self,
	room_id: Option<&'a RoomId>,
	user_id: &'a UserId,
	since: u64,
	to: Option<u64>,
) -> impl Stream<Item = Result<AnyRawAccountDataEvent>> + Send + 'a {
	type Key<'a> = (Option<&'a RoomId>, &'a UserId, u64, Ignore);

	// Skip the data that's exactly at since, because we sent that last time
	let first_possible = (room_id, user_id, since.saturating_add(1));

	self.db
		.roomuserdataid_accountdata
		.stream_from(&first_possible)
		.ready_try_take_while(move |((room_id_, user_id_, count, _), _): &(Key<'_>, _)| {
			Ok(room_id == *room_id_ && user_id == *user_id_ && to.is_none_or(|to| *count <= to))
		})
		.ready_and_then(move |(_, v)| {
			match room_id {
				| Some(_) => serde_json::from_slice::<Raw<AnyRoomAccountDataEvent>>(v)
					.map(AnyRawAccountDataEvent::Room),
				| None => serde_json::from_slice::<Raw<AnyGlobalAccountDataEvent>>(v)
					.map(AnyRawAccountDataEvent::Global),
			}
			.map_err(|e| err!(Database("Database contains invalid account data: {e}")))
		})
}

/// MSC4025: erase all account data for a user in the given namespace
/// (global if `room_id` is `None`, otherwise a single room). Mirrors
/// `threads::delete_all_rooms_threads`: prefix-scan the keys and
/// remove each.
#[implement(Service)]
pub async fn erase_user(&self, user_id: &UserId, room_id: Option<&RoomId>) {
	let prefix = (room_id, user_id, Interfix);
	let mut txn = self.services.db.txn();

	self.db
		.roomuserdataid_accountdata
		.keys_prefix_raw(&prefix)
		.ignore_err()
		.ready_for_each(|key| txn.del_raw(&self.db.roomuserdataid_accountdata, key))
		.await;

	self.db
		.roomusertype_roomuserdataid
		.keys_prefix_raw(&prefix)
		.ignore_err()
		.ready_for_each(|key| txn.del_raw(&self.db.roomusertype_roomuserdataid, key))
		.await;

	txn.execute();
}

/// Returns all changes to the account data that happened after `since`.
#[implement(Service)]
pub async fn last_count<'a>(
	&'a self,
	room_id: Option<&'a RoomId>,
	user_id: &'a UserId,
	upper: Option<u64>,
) -> Result<u64> {
	type Key<'a> = (Option<&'a RoomId>, &'a UserId, u64, Ignore);

	let upper = upper.unwrap_or(u64::MAX);
	let key = (room_id, user_id, upper, Interfix);
	let keys = self
		.db
		.roomuserdataid_accountdata
		.rev_keys_from(&key)
		.ignore_err()
		.ready_take_while(move |(room_id_, user_id_, ..): &Key<'_>| {
			room_id == *room_id_ && user_id == *user_id_
		})
		.map(at!(2));

	pin_mut!(keys);
	keys.next()
		.await
		.ok_or_else(|| err!(Request(NotFound("No account data found."))))
}
