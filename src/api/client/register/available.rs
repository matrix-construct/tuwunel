use axum::extract::State;
use ruma::{UserId, api::client::account::get_username_availability};
use tuwunel_core::{Err, Result};

use super::is_matrix_appservice_irc;
use crate::{ClientIp, Ruma};

/// # `GET /_matrix/client/v3/register/available`
///
/// Checks if a username is valid and available on this server.
///
/// Conditions for returning true:
/// - The user id is not historical
/// - The server name of the user id matches this server
/// - No user or appservice on this server already claimed this username
///
/// Note: This will not reserve the username, so the username might become
/// invalid when trying to register
#[tracing::instrument(skip_all, fields(%client), name = "register_available")]
pub(crate) async fn get_register_available_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<get_username_availability::v3::Request>,
) -> Result<get_username_availability::v3::Response> {
	let is_irc = is_matrix_appservice_irc(body.appservice_info.as_ref());

	if services
		.config
		.registration
		.forbidden_usernames
		.is_match(&body.username)
	{
		return Err!(Request(Forbidden("Username is forbidden")));
	}

	// don't force the username lowercase if it's from matrix-appservice-irc
	let body_username = if is_irc {
		body.username.clone()
	} else {
		body.username.to_lowercase()
	};

	// Validate user id
	let user_id =
		match UserId::parse_with_server_name(&body_username, services.globals.server_name()) {
			| Ok(user_id) => {
				if let Err(e) = user_id.validate_strict() {
					// unless the username is from the broken matrix appservice IRC bridge, we
					// should follow synapse's behaviour on not allowing things like spaces
					// and UTF-8 characters in usernames
					if !is_irc {
						return Err!(Request(InvalidUsername(debug_warn!(
							"Username {body_username} contains disallowed characters or spaces: \
							 {e}"
						))));
					}
				}

				user_id
			},
			| Err(e) => {
				return Err!(Request(InvalidUsername(debug_warn!(
					"Username {body_username} is not valid: {e}"
				))));
			},
		};

	// Check if username is creative enough
	if services.users.exists(&user_id).await {
		return Err!(Request(UserInUse("User ID is not available.")));
	}

	if let Some(ref info) = body.appservice_info
		&& !info.is_user_match(&user_id)
	{
		return Err!(Request(Exclusive("Username is not in an appservice namespace.")));
	}

	if services
		.appservice
		.is_exclusive_user_id(&user_id)
		.await
	{
		return Err!(Request(Exclusive("Username is reserved by an appservice.")));
	}

	Ok(get_username_availability::v3::Response { available: true })
}
