use std::sync::Arc;

use ruma::{RoomId, UserId};
use tuwunel_core::{Result, implement};
use tuwunel_database::{Database, Deserialized, Map};

use crate::{Dep, globals, rooms, rooms::short::ShortStateHash};

pub struct Service {
	db: Data,
	services: Services,
}

struct Data {
	db: Arc<Database>,
	userroomid_notificationcount: Arc<Map>,
	userroomid_highlightcount: Arc<Map>,
	roomuserid_lastnotificationread: Arc<Map>,
	roomsynctoken_shortstatehash: Arc<Map>,
}

struct Services {
	globals: Dep<globals::Service>,
	short: Dep<rooms::short::Service>,
}

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				db: args.db.clone(),
				userroomid_notificationcount: args.db["userroomid_notificationcount"].clone(),
				userroomid_highlightcount: args.db["userroomid_highlightcount"].clone(),
				roomuserid_lastnotificationread: args.db["userroomid_highlightcount"].clone(),
				roomsynctoken_shortstatehash: args.db["roomsynctoken_shortstatehash"].clone(),
			},

			services: Services {
				globals: args.depend::<globals::Service>("globals"),
				short: args.depend::<rooms::short::Service>("rooms::short"),
			},
		}))
	}

	fn name(&self) -> &str {
		crate::service::make_name(std::module_path!())
	}
}

#[implement(Service)]
pub fn reset_notification_counts(&self, user_id: &UserId, room_id: &RoomId) {
	let userroom_id = (user_id, room_id);
	self.db
		.userroomid_highlightcount
		.put(userroom_id, 0_u64);
	self.db
		.userroomid_notificationcount
		.put(userroom_id, 0_u64);

	let roomuser_id = (room_id, user_id);
	let count = self.services.globals.next_count().unwrap();
	self.db
		.roomuserid_lastnotificationread
		.put(roomuser_id, count);
}

#[implement(Service)]
pub async fn notification_count(&self, user_id: &UserId, room_id: &RoomId) -> u64 {
	let key = (user_id, room_id);
	self.db
		.userroomid_notificationcount
		.qry(&key)
		.await
		.deserialized()
		.unwrap_or(0)
}

#[implement(Service)]
pub async fn highlight_count(&self, user_id: &UserId, room_id: &RoomId) -> u64 {
	let key = (user_id, room_id);
	self.db
		.userroomid_highlightcount
		.qry(&key)
		.await
		.deserialized()
		.unwrap_or(0)
}

#[implement(Service)]
pub async fn last_notification_read(&self, user_id: &UserId, room_id: &RoomId) -> u64 {
	let key = (room_id, user_id);
	self.db
		.roomuserid_lastnotificationread
		.qry(&key)
		.await
		.deserialized()
		.unwrap_or(0)
}

#[implement(Service)]
pub async fn associate_token_shortstatehash(
	&self,
	room_id: &RoomId,
	token: u64,
	shortstatehash: ShortStateHash,
) {
	let shortroomid = self
		.services
		.short
		.get_shortroomid(room_id)
		.await
		.expect("room exists");

	let _cork = self.db.db.cork();
	let key: &[u64] = &[shortroomid, token];
	self.db
		.roomsynctoken_shortstatehash
		.put(key, shortstatehash);
}

#[implement(Service)]
pub async fn get_token_shortstatehash(
	&self,
	room_id: &RoomId,
	token: u64,
) -> Result<ShortStateHash> {
	let shortroomid = self
		.services
		.short
		.get_shortroomid(room_id)
		.await?;

	let key: &[u64] = &[shortroomid, token];
	self.db
		.roomsynctoken_shortstatehash
		.qry(key)
		.await
		.deserialized()
}
