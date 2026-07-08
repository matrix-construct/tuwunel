use axum::extract::State;
use synapse_admin_api::devices::create_device::v1 as create_device;
use tuwunel_core::Result;

use super::require_local_user;
use crate::{Ruma, client::admin::require_admin};

/// # `POST /_synapse/admin/v2/users/{user_id}/devices`
///
/// Creating a device that already exists is a no-op.
pub(crate) async fn admin_create_device_route(
	State(services): State<crate::State>,
	body: Ruma<create_device::Request>,
) -> Result<create_device::Response> {
	require_admin(&services, body.sender_user()).await?;

	let user_id = &body.user_id;

	require_local_user(&services, user_id).await?;

	let device_id = &body.device_id;

	if services
		.users
		.device_exists(user_id, device_id)
		.await
	{
		return Ok(create_device::Response {});
	}

	services
		.users
		.create_device(user_id, Some(device_id), (None, None), None, None, None)
		.await?;

	Ok(create_device::Response {})
}
