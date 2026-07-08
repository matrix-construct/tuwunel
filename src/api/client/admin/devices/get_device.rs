use axum::extract::State;
use synapse_admin_api::devices::get_device::v1 as get_device;
use tuwunel_core::{Err, Result};

use super::{device_response, require_local_user};
use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v2/users/{user_id}/devices/{device_id}`
pub(crate) async fn admin_get_device_route(
	State(services): State<crate::State>,
	body: Ruma<get_device::Request>,
) -> Result<get_device::Response> {
	require_admin(&services, body.sender_user()).await?;

	let user_id = &body.user_id;
	require_local_user(&services, user_id).await?;

	let Ok(device) = services
		.users
		.get_device_metadata(user_id, &body.device_id)
		.await
	else {
		return Err!(Request(NotFound("No device found")));
	};

	// The single-device view carries no `dehydrated` flag; it is a list-only
	// field in Synapse.
	Ok(get_device::Response {
		device: device_response(device, user_id, None),
	})
}
