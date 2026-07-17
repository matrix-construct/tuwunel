use axum::extract::State;
use futures::FutureExt;
use ruma::api::client::membership::leave_room;
use tuwunel_core::Result;

use crate::Ruma;

/// # `POST /_matrix/client/v3/rooms/{roomId}/leave`
///
/// Tries to leave the sender user from a room.
///
/// - This should always work if the user is currently joined.
pub(crate) async fn leave_room_route(
	State(services): State<crate::State>,
	body: Ruma<leave_room::v3::Request>,
) -> Result<leave_room::v3::Response> {
	let state_lock = services.state.mutex.lock(&body.room_id).await;

	services
		.membership
		.leave(body.sender_user(), &body.room_id, body.reason.clone(), false, &state_lock)
		.await?;

	if services.config.delete_rooms_after_leave {
		services
			.delete
			.delete_if_empty_local(&body.room_id, state_lock)
			.boxed()
			.await;
	}

	Ok(leave_room::v3::Response {})
}
