use axum::extract::State;
use synapse_admin_api::devices::delete_device::v1 as delete_device;
use tuwunel_core::Result;

use super::require_local_user;
use crate::{Ruma, client::admin::require_admin};

/// # `DELETE /_synapse/admin/v2/users/{user_id}/devices/{device_id}`
///
/// Deleting an unknown device is not an error.
pub(crate) async fn admin_delete_device_route(
	State(services): State<crate::State>,
	body: Ruma<delete_device::Request>,
) -> Result<delete_device::Response> {
	require_admin(&services, body.sender_user()).await?;

	let user_id = &body.user_id;
	require_local_user(&services, user_id).await?;

	services
		.users
		.remove_device(user_id, &body.device_id)
		.await;

	Ok(delete_device::Response {})
}
