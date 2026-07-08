use axum::extract::State;
use synapse_admin_api::registration_tokens::{RegistrationToken, get::v1 as get};
use tuwunel_core::{Err, Result};

use super::token_response;
use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v1/registration_tokens/{token}`
pub(crate) async fn admin_get_token_route(
	State(services): State<crate::State>,
	body: Ruma<get::Request>,
) -> Result<get::Response> {
	require_admin(&services, body.sender_user()).await?;

	let token = &body.token;

	if let Some(info) = services
		.registration_tokens
		.get_token_info(token)
		.await?
	{
		return Ok(get::Response {
			token: token_response(token.clone(), &info),
		});
	}

	if services
		.registration_tokens
		.get_config_tokens()
		.contains(token)
	{
		return Ok(get::Response {
			token: RegistrationToken::new(token.clone()),
		});
	}

	Err!(Request(NotFound("No such registration token")))
}
