mod direct;
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
use tuwunel_core::{
	Err, Result, at, err, implement,
	utils::{ReadyExt, result::LogErr, stream::TryIgnore},
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
	self.db
		.roomuserdataid_accountdata
		.put(roomuserdataid, Json(data));

	let key = (room_id, user_id, &event_type);
	let prev = self
		.db
		.roomusertype_roomuserdataid
		.qry(&key)
		.await;

	self.db
		.roomusertype_roomuserdataid
		.put(key, roomuserdataid);

	// Remove old entry
	if let Ok(prev) = prev {
		self.db.roomuserdataid_accountdata.remove(&prev);
	}

	Ok(())
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
	type Key<'a> = (Option<&'a RoomId>, &'a UserId, u64, Ignore);

	// Skip the data that's exactly at since, because we sent that last time
	let first_possible = (room_id, user_id, since.saturating_add(1));

	self.db
		.roomuserdataid_accountdata
		.stream_from(&first_possible)
		.ignore_err()
		.ready_take_while(move |((room_id_, user_id_, count, _), _): &(Key<'_>, _)| {
			room_id == *room_id_ && user_id == *user_id_ && to.is_none_or(|to| *count <= to)
		})
		.map(move |(_, v)| {
			match room_id {
				| Some(_) => serde_json::from_slice::<Raw<AnyRoomAccountDataEvent>>(v)
					.map(AnyRawAccountDataEvent::Room),
				| None => serde_json::from_slice::<Raw<AnyGlobalAccountDataEvent>>(v)
					.map(AnyRawAccountDataEvent::Global),
			}
			.map_err(|e| err!(Database("Database contains invalid account data: {e}")))
			.log_err()
		})
		.ignore_err()
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
