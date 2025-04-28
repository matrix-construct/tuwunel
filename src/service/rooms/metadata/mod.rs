use std::sync::Arc;

use futures::{Stream, StreamExt};
use ruma::RoomId;
use tuwunel_core::{Result, implement, utils::stream::TryIgnore};
use tuwunel_database::Map;

use crate::{Dep, rooms};

pub struct Service {
	db: Data,
	services: Services,
}

struct Data {
	disabledroomids: Arc<Map>,
	bannedroomids: Arc<Map>,
	roomid_shortroomid: Arc<Map>,
	pduid_pdu: Arc<Map>,
}

struct Services {
	short: Dep<rooms::short::Service>,
}

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				disabledroomids: args.db["disabledroomids"].clone(),
				bannedroomids: args.db["bannedroomids"].clone(),
				roomid_shortroomid: args.db["roomid_shortroomid"].clone(),
				pduid_pdu: args.db["pduid_pdu"].clone(),
			},
			services: Services {
				short: args.depend::<rooms::short::Service>("rooms::short"),
			},
		}))
	}

	fn name(&self) -> &str {
		crate::service::make_name(std::module_path!())
	}
}

#[implement(Service)]
pub async fn exists(&self, room_id: &RoomId) -> bool {
	let Ok(prefix) = self.services.short.get_shortroomid(room_id).await else {
		return false;
	};

	// Look for PDUs in that room.
	self.db
		.pduid_pdu
		.keys_prefix_raw(&prefix)
		.ignore_err()
		.next()
		.await
		.is_some()
}

#[implement(Service)]
pub fn iter_ids(&self) -> impl Stream<Item = &RoomId> + Send + '_ {
	self.db.roomid_shortroomid.keys().ignore_err()
}

#[implement(Service)]
#[inline]
pub fn disable_room(&self, room_id: &RoomId, disabled: bool) {
	if disabled {
		self.db.disabledroomids.insert(room_id, []);
	} else {
		self.db.disabledroomids.remove(room_id);
	}
}

#[implement(Service)]
#[inline]
pub fn ban_room(&self, room_id: &RoomId, banned: bool) {
	if banned {
		self.db.bannedroomids.insert(room_id, []);
	} else {
		self.db.bannedroomids.remove(room_id);
	}
}

#[implement(Service)]
pub fn list_banned_rooms(&self) -> impl Stream<Item = &RoomId> + Send + '_ {
	self.db.bannedroomids.keys().ignore_err()
}

#[implement(Service)]
#[inline]
pub async fn is_disabled(&self, room_id: &RoomId) -> bool {
	self.db.disabledroomids.get(room_id).await.is_ok()
}

#[implement(Service)]
#[inline]
pub async fn is_banned(&self, room_id: &RoomId) -> bool {
	self.db.bannedroomids.get(room_id).await.is_ok()
}
