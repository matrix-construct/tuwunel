use axum::extract::State;
use synapse_admin_api::registration_tokens::create::v1 as create;
use tuwunel_core::Result;

use super::{token_expires, token_response};
use crate::{Ruma, client::admin::require_admin};

/// # `POST /_synapse/admin/v1/registration_tokens/new`
///
/// A duplicate token is rejected with `400`, not `409`.
pub(crate) async fn admin_create_token_route(
	State(services): State<crate::State>,
	body: Ruma<create::Request>,
) -> Result<create::Response> {
	require_admin(&services, body.sender_user()).await?;

	let expires = token_expires(body.uses_allowed, body.expiry_time)?;

	let (token, info) = services
		.registration_tokens
		.create_token(body.token.as_deref(), body.length.map(Into::into), expires)
		.await?;

	Ok(create::Response { token: token_response(token, &info) })
}
