//! Tracks room existence, public visibility, and local moderation markers.
//!
//! Existence is inferred from stored timeline rows, while room IDs come from
//! the short-ID index. Disabled and banned markers have independent lifecycles.

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

/// Provides room inventory and local moderation metadata.
///
/// The service combines short-room and timeline indexes with directory and
/// join-rule state, plus persistent disabled and banned marker maps.
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

/// Reports whether at least one timeline PDU is stored for a room.
///
/// A room without a short ID is absent. Database scan errors are skipped, so a
/// failed or empty scan also returns `false`.
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

/// Streams public room IDs whose encoded IDs begin with a prefix.
///
/// IDs are owned before the asynchronous public-room check. Invalid index rows
/// are skipped, and public means directory-listed or governed by a public join rule.
#[implement(Service)]
pub fn public_ids_prefix<'a>(
	&'a self,
	prefix: &'a str,
) -> impl Stream<Item = OwnedRoomId> + Send + 'a {
	self.ids_prefix(prefix)
		.map(ToOwned::to_owned)
		.wide_filter_map(async |room_id| self.is_public(&room_id).await.then_some(room_id))
}

/// Streams indexed room IDs whose encoded IDs begin with a prefix.
///
/// Each borrowed ID is valid only until the stream is polled again and must be
/// copied before retention. Unparsable rows are skipped.
#[implement(Service)]
pub fn ids_prefix<'a>(&'a self, prefix: &'a str) -> impl Stream<Item = &RoomId> + Send + 'a {
	self.db
		.roomid_shortroomid
		.keys_raw_prefix(prefix)
		.ignore_err()
}

/// Streams every room ID present in the short-room-ID index.
///
/// Indexed rooms need not still contain timeline events. Each borrowed ID is
/// valid only until the next poll, and unparsable rows are skipped.
#[implement(Service)]
pub fn iter_ids(&self) -> impl Stream<Item = &RoomId> + Send + '_ {
	self.db.roomid_shortroomid.keys().ignore_err()
}

/// Reports whether a room is publicly discoverable or joinable.
///
/// An explicit directory listing or a valid public join rule is sufficient.
/// Missing or invalid join-rule state falls back to invite and does not grant access.
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

/// Disables inbound federation processing for a room.
///
/// The persistent marker is independent of the room's banned status and may be
/// written before the server has stored the room.
#[implement(Service)]
#[inline]
pub fn disable_room(&self, room_id: &RoomId) { self.db.disabledroomids.insert(room_id, []); }

/// Re-enables inbound federation processing for a room.
///
/// Removing the disabled marker does not alter any banned marker for the room.
#[implement(Service)]
#[inline]
pub fn enable_room(&self, room_id: &RoomId) { self.db.disabledroomids.remove(room_id); }

/// Bans a room without recording a responsible user.
///
/// The empty marker overwrites any previously stored blocker attribution. A
/// room may be banned before any of its events are stored locally.
#[implement(Service)]
#[inline]
pub fn ban_room(&self, room_id: &RoomId) { self.db.bannedroomids.insert(room_id, []); }

/// Removes a room's local ban marker.
///
/// Unbanning does not re-enable federation if the independent disabled marker
/// remains present.
#[implement(Service)]
#[inline]
pub fn unban_room(&self, room_id: &RoomId) { self.db.bannedroomids.remove(room_id); }

/// Bans a room and records the user responsible for the block.
///
/// The user ID replaces any existing empty or attributed ban value. The room
/// need not otherwise exist locally.
#[implement(Service)]
#[inline]
pub fn block_room(&self, room_id: &RoomId, blocker: &UserId) {
	self.db
		.bannedroomids
		.insert(room_id, blocker.as_bytes());
}

/// Returns the user attributed to a room's ban marker.
///
/// Empty legacy or unattributed markers, invalid user IDs, missing rows, and
/// database errors all yield `None`; use [`Self::is_banned`] to test the marker.
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

/// Streams every room with a local ban marker.
///
/// Each borrowed room ID is valid only until the stream is polled again and
/// must be copied before retention. Unparsable rows are skipped.
#[implement(Service)]
pub fn list_banned_rooms(&self) -> impl Stream<Item = &RoomId> + Send + '_ {
	self.db.bannedroomids.keys().ignore_err()
}

/// Reports whether a room has a disabled marker.
///
/// Missing rows and database errors both return `false`.
#[implement(Service)]
#[inline]
pub async fn is_disabled(&self, room_id: &RoomId) -> bool {
	self.db.disabledroomids.get(room_id).await.is_ok()
}

/// Reports whether a room has a banned marker.
///
/// Both empty bans and user-attributed blocks count. Missing rows and database
/// errors return `false`.
#[implement(Service)]
#[inline]
pub async fn is_banned(&self, room_id: &RoomId) -> bool {
	self.db.bannedroomids.get(room_id).await.is_ok()
}
