use axum::extract::State;
use synapse_admin_api::users::deactivate_account::v1 as deactivate_account;
use tuwunel_core::{Err, Result};

use crate::{Ruma, client::admin::require_admin};

/// # `POST /_synapse/admin/v1/deactivate/{user_id}`
pub(crate) async fn admin_deactivate_account_route(
	State(services): State<crate::State>,
	body: Ruma<deactivate_account::Request>,
) -> Result<deactivate_account::Response> {
	require_admin(&services, body.sender_user()).await?;

	if !services.globals.user_is_local(&body.user_id) {
		return Err!(Request(InvalidParam("Can only deactivate local users")));
	}

	if !services.users.exists(&body.user_id).await {
		return Err!(Request(NotFound("User not found")));
	}

	services
		.users
		.deactivate_account(&body.user_id)
		.await?;

	if body.erase {
		services.users.set_erased(&body.user_id);
	}

	Ok(deactivate_account::Response::new("success".to_owned()))
}
