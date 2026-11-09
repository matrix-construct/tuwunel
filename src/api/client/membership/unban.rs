use axum::extract::State;
use ruma::api::client::membership::unban_user;
use tuwunel_core::Result;

use crate::Ruma;

/// # `POST /_matrix/client/r0/rooms/{roomId}/unban`
///
/// Tries to send an unban event into the room.
pub(crate) async fn unban_user_route(
	State(services): State<crate::State>,
	body: Ruma<unban_user::v3::Request>,
) -> Result<unban_user::v3::Response> {
	let state_lock = services.state.mutex.lock(&body.room_id).await;

	services
		.membership
		.unban(
			&body.room_id,
			&body.user_id,
			body.reason.as_ref(),
			body.sender_user(),
			&state_lock,
		)
		.await?;

	drop(state_lock);

	Ok(unban_user::v3::Response::new())
}
