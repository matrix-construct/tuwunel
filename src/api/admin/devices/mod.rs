//! Synapse admin API: device endpoints.

mod create_device;
mod delete_device;
mod delete_devices;
mod get_device;
mod list_devices;
mod update_device;

use ruma::{
	DeviceId, UserId,
	api::client::device::{Device as ClientDevice, DisplayName, LastSeenIp},
};
use synapse_admin_api::devices::Device;
use tuwunel_core::{Err, Result, err};

pub(crate) use self::{
	create_device::admin_create_device_route, delete_device::admin_delete_device_route,
	delete_devices::admin_delete_devices_route, get_device::admin_get_device_route,
	list_devices::admin_list_devices_route, update_device::admin_update_device_route,
};

/// Reject a non-local user with `400` and an absent one with `404`, mirroring
/// Synapse's device servlets.
pub(super) async fn require_local_user(services: &crate::State, user_id: &UserId) -> Result<()> {
	if !services.globals.user_is_local(user_id) {
		return Err!(Request(InvalidParam("Can only lookup local users")));
	}

	services
		.users
		.exists(user_id)
		.await
		.then_some(())
		.ok_or_else(|| err!(Request(NotFound("Unknown user"))))
}

/// Map a stored device into the admin API wire shape. Tuwunel keeps no
/// user-agent, so `last_seen_user_agent` is always absent; `dehydrated` is
/// present on every device only when the owner has a dehydrated one.
pub(super) fn device_response(
	device: ClientDevice,
	user_id: &UserId,
	dehydrated: Option<&DeviceId>,
) -> Device {
	Device {
		dehydrated: dehydrated.map(|id| device.device_id == id),
		device_id: device.device_id,
		display_name: device.display_name.map(DisplayName::into_string),
		user_id: user_id.to_owned(),
		last_seen_ip: device.last_seen_ip.map(LastSeenIp::into_string),
		last_seen_user_agent: None,
		last_seen_ts: device.last_seen_ts,
	}
}
