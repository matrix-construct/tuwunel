use axum::extract::State;
use ruma::api::client::typing::create_typing_event;
use tuwunel_core::{Err, Result, utils, utils::math::Tried};

use crate::Ruma;

/// # `PUT /_matrix/client/r0/rooms/{roomId}/typing/{userId}`
///
/// Sets the typing state of the sender user.
pub(crate) async fn create_typing_event_route(
	State(services): State<crate::State>,
	body: Ruma<create_typing_event::v3::Request>,
) -> Result<create_typing_event::v3::Response> {
	use create_typing_event::v3::Typing;
	let sender_user = body.sender_user();

	if sender_user != body.user_id && body.appservice_info.is_none() {
		return Err!(Request(Forbidden("You cannot update typing status of other users.")));
	}

	if !services
		.state_cache
		.is_joined(sender_user, &body.room_id)
		.await
	{
		return Err!(Request(Forbidden("You are not in this room.")));
	}

	match body.state {
		| Typing::Yes(duration) => {
			let duration = Ord::clamp(
				duration
					.as_millis()
					.try_into()
					.unwrap_or(u64::MAX),
				services
					.server
					.config
					.typing_client_timeout_min_s
					.try_mul(1000)?,
				services
					.server
					.config
					.typing_client_timeout_max_s
					.try_mul(1000)?,
			);
			services
				.typing
				.typing_add(
					sender_user,
					&body.room_id,
					utils::millis_since_unix_epoch()
						.checked_add(duration)
						.expect("user typing timeout should not get this high"),
				)
				.await?;
		},
		| _ => {
			services
				.typing
				.typing_remove(sender_user, &body.room_id)
				.await?;
		},
	}

	// ping presence
	services
		.presence
		.maybe_ping_presence(
			&body.user_id,
			body.sender_device.as_deref(),
			&ruma::presence::PresenceState::Online,
		)
		.await?;

	Ok(create_typing_event::v3::Response {})
}
