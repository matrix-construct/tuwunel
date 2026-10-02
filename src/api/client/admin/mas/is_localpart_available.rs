use axum::extract::State;
use ruma::UserId;
use synapse_admin_api::mas::is_localpart_available::{Request, Response};
use tuwunel_core::{Err, Result, err};

use super::Mas;
use crate::Ruma;

/// # `GET /_synapse/mas/is_localpart_available`
pub(crate) async fn is_localpart_available_route(
	_mas: Mas,
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	if services
		.config
		.forbidden_usernames
		.is_match(&body.localpart)
	{
		return Err!(Request(InvalidUsername("Localpart is forbidden")));
	}

	let user_id = UserId::parse_with_server_name(&body.localpart, services.globals.server_name())
		.map_err(|_| err!(Request(InvalidUsername("Localpart is not a valid username"))))?;

	if user_id.validate_strict().is_err() {
		return Err!(Request(InvalidUsername("Localpart contains disallowed characters")));
	}

	if services.users.exists(&user_id).await {
		return Err!(Request(UserInUse("Localpart is not available")));
	}

	services.users.check_creation(&user_id).await?;

	if services
		.appservice
		.is_exclusive_user_id(&user_id)
		.await
	{
		return Err!(Request(Exclusive("Localpart is reserved by an appservice")));
	}

	Ok(Response::new())
}
