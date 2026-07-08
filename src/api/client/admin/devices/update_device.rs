use axum::extract::State;
use ruma::api::client::device::{Device, DisplayName};
use synapse_admin_api::devices::update_device::v1 as update_device;
use tuwunel_core::{Err, Result};

use super::require_local_user;
use crate::{Ruma, client::admin::require_admin};

/// Synapse's `MAX_DEVICE_DISPLAY_NAME_LEN`.
const MAX_DISPLAY_NAME_LEN: usize = 100;

/// # `PUT /_synapse/admin/v2/users/{user_id}/devices/{device_id}`
///
/// Only `display_name` is honored; an omitted one leaves it unchanged.
pub(crate) async fn admin_update_device_route(
	State(services): State<crate::State>,
	body: Ruma<update_device::Request>,
) -> Result<update_device::Response> {
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

	let Some(display_name) = &body.display_name else {
		return Ok(update_device::Response {});
	};

	if display_name.chars().count() > MAX_DISPLAY_NAME_LEN {
		return Err!(Request(InvalidParam("Device display name is too long")));
	}

	let display_name = DisplayName::from(display_name.as_str());
	let notify = device.display_name.as_ref() != Some(&display_name);
	let device = Device {
		display_name: Some(display_name),
		..device
	};

	services
		.users
		.put_device_metadata(user_id, notify, &device);

	Ok(update_device::Response {})
}
