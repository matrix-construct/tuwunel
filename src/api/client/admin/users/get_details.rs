use axum::extract::State;
use synapse_admin_api::users::get_details::v2 as get_details;
use tuwunel_core::{Err, Result};

use super::user_details;
use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v2/users/{user_id}`
pub(crate) async fn admin_get_details_route(
	State(services): State<crate::State>,
	body: Ruma<get_details::Request>,
) -> Result<get_details::Response> {
	require_admin(&services, body.sender_user()).await?;

	if !services.globals.user_is_local(&body.user_id) {
		return Err!(Request(InvalidParam("Can only look up local users")));
	}

	if !services.users.exists(&body.user_id).await {
		return Err!(Request(NotFound("User not found")));
	}

	let details = user_details(services, &body.user_id).await;

	Ok(get_details::Response::new(details))
}
