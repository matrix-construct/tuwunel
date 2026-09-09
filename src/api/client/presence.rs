use std::time::Duration;

use axum::extract::State;
use ruma::{
	api::client::presence::{get_presence, get_presence::v3::Response, set_presence},
	events::presence::PresenceEvent,
	presence::PresenceState,
};
use tuwunel_core::{Err, Result, err};

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

/// Gets the presence state of the given user.
///
/// Access requires the user's own identity or a shared room. Missing state
/// defaults to offline for self or a known local user after authorization.
pub(crate) async fn get_presence_route(
	State(services): State<crate::State>,
	body: Ruma<get_presence::v3::Request>,
) -> Result<Response> {
	if !services.config.allow_local_presence {
		return Err!(Request(Forbidden("Presence is disabled on this server",)));
	}

	let own_presence = body.sender_user() == body.user_id;

	if !own_presence
		&& !services
			.state_cache
			.user_sees_user(body.sender_user(), &body.user_id)
			.await
	{
		return Err!(Request(NotFound("Presence state for this user was not found")));
	}

	match services
		.presence
		.get_presence_optional(&body.user_id)
		.await?
	{
		| Some(presence) => Ok(into_response(presence)),
		| None => {
			let known_local = !own_presence
				&& services.globals.user_is_local(&body.user_id)
				&& services.users.exists(&body.user_id).await;

			may_default_presence(own_presence, known_local)
				.then(default_presence)
				.ok_or_else(|| {
					err!(Request(NotFound("Presence state for this user was not found")))
				})
		},
	}
}

fn into_response(presence: PresenceEvent) -> Response {
	let content = presence.content;
	let status_msg = content
		.status_msg
		.filter(|status| !status.is_empty());

	let last_active_ago = content
		.last_active_ago
		.filter(|_| content.currently_active != Some(true))
		.map(|millis| Duration::from_millis(millis.into()));

	Response {
		status_msg,
		currently_active: content.currently_active,
		last_active_ago,
		presence: content.presence,
	}
}

#[inline]
fn may_default_presence(own_presence: bool, known_local: bool) -> bool {
	own_presence || known_local
}

#[inline]
fn default_presence() -> Response {
	Response {
		status_msg: None,
		currently_active: None,
		last_active_ago: None,
		presence: PresenceState::Offline,
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use ruma::presence::PresenceState;
	use serde_json::{from_value, json};

	use super::{default_presence, into_response, may_default_presence};

	#[test]
	fn missing_presence_fallback_is_private() {
		assert!(may_default_presence(true, false));
		assert!(may_default_presence(false, true));
		assert!(may_default_presence(true, true));
		assert!(!may_default_presence(false, false));
	}

	#[test]
	fn missing_presence_defaults_to_unobserved_offline() {
		let response = default_presence();

		assert_eq!(response.presence, PresenceState::Offline);
		assert_eq!(response.currently_active, None);
		assert_eq!(response.last_active_ago, None);
		assert_eq!(response.status_msg, None);
	}

	#[test]
	fn stored_presence_preserves_status_and_activity() {
		let cases = [
			(None, None, None, None, None),
			(None, Some("busy"), Some(42), Some("busy"), Some(42)),
			(Some(false), Some(""), Some(42), None, Some(42)),
			(Some(true), Some("busy"), Some(42), Some("busy"), None),
		];

		for (active, status, age, expected_status, expected_age) in cases {
			let event = json!({
				"sender": "@user:example.com",
				"type": "m.presence",
				"content": {
					"presence": "online",
					"currently_active": active,
					"status_msg": status,
					"last_active_ago": age,
				},
			});

			let response = into_response(from_value(event).expect("valid presence event"));

			assert_eq!(response.presence, PresenceState::Online);
			assert_eq!(response.currently_active, active);
			assert_eq!(response.status_msg.as_deref(), expected_status);
			assert_eq!(response.last_active_ago, expected_age.map(Duration::from_millis));
		}
	}
}
