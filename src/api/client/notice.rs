use futures::TryFutureExt;
use ruma::{
	RoomId, UserId,
	events::{StateEventType, tag::TagName},
};
use tuwunel_core::{Result, utils::BoolExt};
use tuwunel_matrix::Event;
use tuwunel_service::Services;

/// Tests whether a room is the target user's server-notice room.
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

/// Checks server membership, the target's tag, and the immutable room creator.
///
/// An absent tag is an ordinary non-notice room; other lookup failures propagate.
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
		.await
		.or_else(|error| error.is_not_found().then_ok_or(false, error))?;

	if !tagged {
		return Ok(false);
	}

	services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomCreate, "")
		.map_ok(|create| create.sender() == services.globals.server_user)
		.await
}
