use axum::extract::State;
use synapse_admin_api::registration_tokens::delete::v1 as delete;
use tuwunel_core::Result;

use crate::{Ruma, client::admin::require_admin};

/// # `DELETE /_synapse/admin/v1/registration_tokens/{token}`
pub(crate) async fn admin_delete_token_route(
	State(services): State<crate::State>,
	body: Ruma<delete::Request>,
) -> Result<delete::Response> {
	require_admin(&services, body.sender_user()).await?;

	services
		.registration_tokens
		.revoke_token(&body.token)
		.await?;

	Ok(delete::Response {})
}
