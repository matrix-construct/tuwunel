use axum::extract::State;
use futures::StreamExt;
use synapse_admin_api::users::reset_password::v1 as reset_password;
use tuwunel_core::{Err, Result, utils::stream::automatic_width};

use crate::{Ruma, client::admin::require_admin};

/// # `POST /_synapse/admin/v1/reset_password/{user_id}`
pub(crate) async fn admin_reset_password_route(
	State(services): State<crate::State>,
	body: Ruma<reset_password::Request>,
) -> Result<reset_password::Response> {
	require_admin(&services, body.sender_user()).await?;

	if !services.users.exists(&body.user_id).await {
		return Err!(Request(NotFound("User not found")));
	}

	services
		.users
		.set_password(&body.user_id, Some(&body.new_password))
		.await?;

	if body.logout_devices {
		services
			.users
			.all_device_ids(&body.user_id)
			.map(ToOwned::to_owned)
			.for_each_concurrent(automatic_width(), async |device_id| {
				services
					.users
					.remove_device(&body.user_id, &device_id)
					.await;
			})
			.await;
	}

	Ok(reset_password::Response::new())
}
