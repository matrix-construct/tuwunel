use axum::extract::State;
use synapse_admin_api::users::is_user_admin::v1 as is_user_admin;
use tuwunel_core::{Err, Result};

use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v1/users/{user_id}/admin`
pub(crate) async fn admin_is_user_admin_route(
	State(services): State<crate::State>,
	body: Ruma<is_user_admin::Request>,
) -> Result<is_user_admin::Response> {
	require_admin(&services, body.sender_user()).await?;

	if !services.globals.user_is_local(&body.user_id) {
		return Err!(Request(InvalidParam("Can only look up local users")));
	}

	let admin = services.admin.user_is_admin(&body.user_id).await;

	Ok(is_user_admin::Response::new(admin))
}
