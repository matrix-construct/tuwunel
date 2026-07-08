use std::sync::Arc;

use futures::{FutureExt, Stream, StreamExt, pin_mut};
use ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId, events::room::join_rules::JoinRule};
use tuwunel_core::{
	Result, implement,
	utils::{
		future::BoolExt,
		stream::{TryIgnore, WidebandExt},
	},
};
use tuwunel_database::Map;

pub struct Service {
	db: Data,
	services: Arc<crate::services::OnceServices>,
}

struct Data {
	disabledroomids: Arc<Map>,
	bannedroomids: Arc<Map>,
	roomid_shortroomid: Arc<Map>,
	pduid_pdu: Arc<Map>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				disabledroomids: args.db["disabledroomids"].clone(),
				bannedroomids: args.db["bannedroomids"].clone(),
				roomid_shortroomid: args.db["roomid_shortroomid"].clone(),
				pduid_pdu: args.db["pduid_pdu"].clone(),
			},
			services: args.services.clone(),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

#[implement(Service)]
pub async fn exists(&self, room_id: &RoomId) -> bool {
	let Ok(prefix) = self.services.short.get_shortroomid(room_id).await else {
		return false;
	};

	// Look for PDUs in that room.
	let keys = self
		.db
		.pduid_pdu
		.keys_prefix_raw(&prefix)
		.ignore_err();

	pin_mut!(keys);
	keys.next().await.is_some()
}

#[implement(Service)]
pub fn public_ids_prefix<'a>(
	&'a self,
	prefix: &'a str,
) -> impl Stream<Item = OwnedRoomId> + Send + 'a {
	self.ids_prefix(prefix)
		.map(ToOwned::to_owned)
		.wide_filter_map(async |room_id| self.is_public(&room_id).await.then_some(room_id))
}

#[implement(Service)]
pub fn ids_prefix<'a>(&'a self, prefix: &'a str) -> impl Stream<Item = &RoomId> + Send + 'a {
	self.db
		.roomid_shortroomid
		.keys_raw_prefix(prefix)
		.ignore_err()
}

#[implement(Service)]
pub fn iter_ids(&self) -> impl Stream<Item = &RoomId> + Send + '_ {
	self.db.roomid_shortroomid.keys().ignore_err()
}

#[implement(Service)]
pub async fn is_public(&self, room_id: &RoomId) -> bool {
	let listed_public = self.services.directory.is_public_room(room_id);

	let join_rule_public = self
		.services
		.state_accessor
		.get_join_rules(room_id)
		.map(|rule| matches!(rule, JoinRule::Public));

	pin_mut!(listed_public, join_rule_public);
	listed_public.or(join_rule_public).await
}

#[implement(Service)]
#[inline]
pub fn disable_room(&self, room_id: &RoomId) { self.db.disabledroomids.insert(room_id, []); }

#[implement(Service)]
#[inline]
pub fn enable_room(&self, room_id: &RoomId) { self.db.disabledroomids.remove(room_id); }

#[implement(Service)]
#[inline]
pub fn ban_room(&self, room_id: &RoomId) { self.db.bannedroomids.insert(room_id, []); }

#[implement(Service)]
#[inline]
pub fn unban_room(&self, room_id: &RoomId) { self.db.bannedroomids.remove(room_id); }

#[implement(Service)]
#[inline]
pub fn block_room(&self, room_id: &RoomId, blocker: &UserId) {
	self.db
		.bannedroomids
		.insert(room_id, blocker.as_bytes());
}

#[implement(Service)]
pub async fn banned_room_blocker(&self, room_id: &RoomId) -> Option<OwnedUserId> {
	self.db
		.bannedroomids
		.get(room_id)
		.await
		.ok()
		.and_then(|blocker| {
			str::from_utf8(&blocker)
				.ok()
				.and_then(|mxid| UserId::parse(mxid).ok())
		})
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
