use axum::extract::State;
use futures::{FutureExt, StreamExt, TryFutureExt, future::join4};
use ruma::{
	RoomId, UInt,
	events::{StateEventType, room::tombstone::RoomTombstoneEventContent},
	uint,
};
use synapse_admin_api::rooms::room_details::v1::{Request, Response};
use tuwunel_core::{
	Err, Result,
	utils::{
		TryFutureExtExt,
		stream::{BroadbandExt, ReadyExt},
	},
};
use tuwunel_service::Services;

use super::{room_row, usize_to_uint};
use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v1/rooms/{room_id}`
///
/// Reports the shared room summary plus the details-only fields: topic, avatar,
/// local device count, forgotten flag, and tombstone replacement.
pub(crate) async fn admin_room_details_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	require_admin(&services, body.sender_user()).await?;

	let room_id: &RoomId = &body.room_id;

	if !services.metadata.exists(room_id).await {
		return Err!(Request(NotFound("Room not found")));
	}

	let row = room_row(&services, room_id);

	let topic = services
		.state_accessor
		.get_room_topic(room_id)
		.ok();

	let avatar = services
		.state_accessor
		.get_avatar(room_id)
		.map_ok(|content| content.url)
		.map(|url| url.ok().flatten());

	let tombstone = services
		.state_accessor
		.room_state_get_content(room_id, &StateEventType::RoomTombstone, "")
		.map(|content: Result<RoomTombstoneEventContent>| content.ok());

	let (row, topic, avatar, tombstone) = join4(row, topic, avatar, tombstone).boxed().await;

	let joined_local_devices = local_device_count(&services, room_id).await;

	let forgotten = row.joined_local_members == uint!(0)
		&& services
			.state_cache
			.room_useroncejoined(room_id)
			.ready_any(|user_id| services.globals.user_is_local(user_id))
			.await;

	let (tombstoned, replacement_room) = tombstone
		.map(|content| (true, Some(content.replacement_room)))
		.unwrap_or((false, None));

	Ok(Response {
		room_id: row.room_id,
		name: row.name,
		topic,
		avatar,
		canonical_alias: row.canonical_alias,
		joined_members: row.joined_members,
		joined_local_members: row.joined_local_members,
		joined_local_devices,
		version: row.version,
		creator: row.creator,
		encryption: row.encryption,
		federatable: row.federatable,
		public: row.public,
		join_rules: row.join_rules,
		guest_access: row.guest_access,
		history_visibility: row.history_visibility,
		state_events: row.state_events,
		room_type: row.room_type,
		forgotten,
		tombstoned,
		replacement_room,
	})
}

/// Sums the device counts of every local user currently joined to the room.
async fn local_device_count(services: &Services, room_id: &RoomId) -> UInt {
	let count: usize = services
		.state_cache
		.local_users_in_room(room_id)
		.map(ToOwned::to_owned)
		.broad_then(async |user_id| {
			services
				.users
				.all_device_ids(&user_id)
				.count()
				.await
		})
		.ready_fold(0_usize, usize::saturating_add)
		.await;

	usize_to_uint(count)
}
