use axum::extract::State;
use ruma::MilliSecondsSinceUnixEpoch;
use synapse_admin_api::users::allow_cross_signing_replacement::v1 as allow_cross_signing_replacement;
use tuwunel_core::{Err, Result, err};

use crate::{Ruma, client::admin::require_admin};

/// # `POST /_synapse/admin/v1/users/{user_id}/_allow_cross_signing_replacement_without_uia`
///
/// Built for MAS to call rather than admins directly, so it stays registered
/// under MAS.
pub(crate) async fn admin_allow_cross_signing_replacement_route(
	State(services): State<crate::State>,
	body: Ruma<allow_cross_signing_replacement::Request>,
) -> Result<allow_cross_signing_replacement::Response> {
	require_admin(&services, body.sender_user()).await?;

	if !services.users.exists(&body.user_id).await {
		return Err!(Request(NotFound("User not found")));
	}

	let deadline = services
		.users
		.allow_cross_signing_replacement(&body.user_id);

	let deadline = MilliSecondsSinceUnixEpoch::from_system_time(deadline)
		.ok_or_else(|| err!(Request(InvalidParam("Deadline out of range"))))?;

	Ok(allow_cross_signing_replacement::Response::new(deadline))
}
