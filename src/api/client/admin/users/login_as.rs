use std::time::{Duration, SystemTime};

use axum::extract::State;
use ruma::MilliSecondsSinceUnixEpoch;
use synapse_admin_api::users::login_as::v1::{Request, Response};
use tuwunel_core::{Err, Result};

use crate::{Ruma, client::admin::require_admin};

/// # `POST /_synapse/admin/v1/users/{user_id}/login`
///
/// Mints a token through a visible device on the target account. The target's
/// logout revokes it, and an optional expiry is enforced during authentication.
/// This route is unavailable while Matrix Authentication Service is active.
pub(crate) async fn admin_login_as_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	require_admin(&services, body.sender_user()).await?;

	if !services.globals.user_is_local(&body.user_id) {
		return Err!(Request(InvalidParam("Only local users can be logged in as")));
	}

	if body.sender_user() == body.user_id {
		return Err!(Request(InvalidParam("Cannot use admin API to login as self")));
	}

	if !services.users.exists(&body.user_id).await {
		return Err!(Request(NotFound("User not found")));
	}

	let expires_in = body
		.valid_until_ms
		.and_then(MilliSecondsSinceUnixEpoch::to_system_time)
		.map(|valid_until| {
			valid_until
				.duration_since(SystemTime::now())
				.unwrap_or(Duration::ZERO)
		});

	let (access_token, _) = services.users.generate_access_token(false);

	services
		.users
		.create_device(
			&body.user_id,
			None,
			(Some(&access_token), expires_in),
			None,
			Some("Admin login"),
			None,
		)
		.await?;

	Ok(Response::new(access_token))
}
