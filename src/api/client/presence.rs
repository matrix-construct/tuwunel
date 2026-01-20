use std::time::Duration;

use axum::extract::State;
use ruma::api::client::presence::{get_presence, set_presence};
use tuwunel_core::{Err, Result};

use crate::Ruma;

/// # `PUT /_matrix/client/r0/presence/{userId}/status`
///
/// Sets the presence state of the sender user.
pub(crate) async fn set_presence_route(
	State(services): State<crate::State>,
	body: Ruma<set_presence::v3::Request>,
) -> Result<set_presence::v3::Response> {
	if !services.config.allow_local_presence {
		return Err!(Request(Forbidden("Presence is disabled on this server")));
	}

	if body.sender_user() != body.user_id && body.appservice_info.is_none() {
		return Err!(Request(InvalidParam("Not allowed to set presence of other users")));
	}

	services
		.presence
		.set_presence_for_device(
			body.sender_user(),
			body.sender_device.as_deref(),
			&body.presence,
			body.status_msg.clone(),
		)
		.await?;

	Ok(set_presence::v3::Response {})
}

/// # `GET /_matrix/client/r0/presence/{userId}/status`
///
/// Gets the presence state of the given user.
///
/// - Only works if you share a room with the user
pub(crate) async fn get_presence_route(
	State(services): State<crate::State>,
	body: Ruma<get_presence::v3::Request>,
) -> Result<get_presence::v3::Response> {
	if !services.config.allow_local_presence {
		return Err!(Request(Forbidden("Presence is disabled on this server",)));
	}

	let mut presence_event = None;
	let has_shared_rooms = services
		.state_cache
		.user_sees_user(body.sender_user(), &body.user_id)
		.await;

	if has_shared_rooms
		&& let Ok(presence) = services
			.presence
			.get_presence(&body.user_id)
			.await
	{
		presence_event = Some(presence);
	}

	match presence_event {
		| Some(presence) => {
			let status_msg = if presence
				.content
				.status_msg
				.as_ref()
				.is_some_and(String::is_empty)
			{
				None
			} else {
				presence.content.status_msg
			};

			let last_active_ago = match presence.content.currently_active {
				| Some(true) => None,
				| _ => presence
					.content
					.last_active_ago
					.map(|millis| Duration::from_millis(millis.into())),
			};

			Ok(get_presence::v3::Response {
				// TODO: Should ruma just use the presenceeventcontent type here?
				status_msg,
				currently_active: presence.content.currently_active,
				last_active_ago,
				presence: presence.content.presence,
			})
		},
		| _ => Err!(Request(NotFound("Presence state for this user was not found"))),
	}
}
