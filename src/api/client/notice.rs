use futures::{TryFutureExt, future::ready};
use ruma::{
	RoomId, UserId,
	events::{StateEventType, tag::TagName},
};
use tuwunel_core::{Result, utils::BoolExt};
use tuwunel_matrix::Event;
use tuwunel_service::Services;

/// Tests whether a room is the target user's server-notice room.
///
/// The admin room is excluded first, since the marker check also accepts it
/// whenever the target's stored tag on it is `m.server_notice`, as the default
/// admin room tag writes.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) async fn is_notice_room(
	services: &Services,
	target: &UserId,
	room_id: &RoomId,
) -> Result<bool> {
	if services.admin.is_admin_room(room_id).await {
		return Ok(false);
	}

	room_is_notice(services, target, room_id).await
}

/// Checks server membership, the target's `m.server_notice` tag, and the
/// immutable room creator.
///
/// The admin room also passes whenever the target's stored tag on it is
/// `m.server_notice`, so a caller must exclude it. An absent tag is an ordinary
/// non-notice room; other lookup failures propagate.
#[tracing::instrument(level = "trace", skip_all)]
pub async fn room_is_notice(
	services: &Services,
	target: &UserId,
	room_id: &RoomId,
) -> Result<bool> {
	if !services
		.state_cache
		.is_joined(&services.globals.server_user, room_id)
		.await
	{
		return Ok(false);
	}

	let tagged = services
		.account_data
		.get_room_tags(target, room_id)
		.map_ok(|tags| tags.contains_key(&TagName::ServerNotice))
		.or_else(|error| ready(error.is_not_found().then_ok_or(false, error)))
		.await?;

	if !tagged {
		return Ok(false);
	}

	services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomCreate, "")
		.map_ok(|create| create.sender() == services.globals.server_user)
		.await
}
