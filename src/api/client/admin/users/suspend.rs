use axum::extract::State;
use synapse_admin_api::users::suspend::v1 as suspend;
use tuwunel_core::{Err, Result};

use crate::{Ruma, client::admin::require_admin};

/// # `PUT /_synapse/admin/v1/suspend/{user_id}`
///
/// Registered unconditionally and works under MAS. Distinct from the
/// MSC4323 client-server suspend endpoint.
pub(crate) async fn admin_suspend_route(
	State(services): State<crate::State>,
	body: Ruma<suspend::Request>,
) -> Result<suspend::Response> {
	let sender_user = body.sender_user();

	require_admin(&services, sender_user).await?;

	if !services.globals.user_is_local(&body.user_id) {
		return Err!(Request(InvalidParam("Can only suspend local users")));
	}

	if !services.users.exists(&body.user_id).await {
		return Err!(Request(NotFound("User not found")));
	}

	if services.users.is_suspended(&body.user_id).await != body.suspend {
		match body.suspend {
			| true => services
				.users
				.set_suspended(&body.user_id, sender_user),
			| false => services.users.clear_suspended(&body.user_id),
		}
	}

	let result = suspend::Suspended::new(body.user_id.clone(), body.suspend);

	Ok(suspend::Response::new(result))
}
