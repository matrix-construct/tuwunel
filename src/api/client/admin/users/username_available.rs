use axum::extract::State;
use ruma::UserId;
use synapse_admin_api::username_available::v1 as username_available;
use tuwunel_core::{Err, Result, err};

use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v1/username_available`
///
/// Never returns `available: false`; an unavailable username is always an
/// error, mirroring the client-server `/register/available` endpoint.
pub(crate) async fn admin_username_available_route(
	State(services): State<crate::State>,
	body: Ruma<username_available::Request>,
) -> Result<username_available::Response> {
	require_admin(&services, body.sender_user()).await?;

	if services
		.config
		.forbidden_usernames
		.is_match(&body.username)
	{
		return Err!(Request(InvalidUsername("Username is forbidden")));
	}

	let user_id = UserId::parse_with_server_name(&body.username, services.globals.server_name())
		.map_err(|_| err!(Request(InvalidUsername("Username is not a valid localpart"))))?;

	if user_id.validate_strict().is_err() {
		return Err!(Request(InvalidUsername("Username contains disallowed characters")));
	}

	if services.users.exists(&user_id).await {
		return Err!(Request(UserInUse("Username is not available")));
	}

	if services
		.appservice
		.is_exclusive_user_id(&user_id)
		.await
	{
		return Err!(Request(Exclusive("Username is reserved by an appservice")));
	}

	Ok(username_available::Response::new(true))
}
