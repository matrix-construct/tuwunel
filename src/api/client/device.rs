use axum::extract::State;
use axum_client_ip::InsecureClientIp;
use futures::StreamExt;
use ruma::{
	MilliSecondsSinceUnixEpoch,
	api::client::device::{
		self, delete_device, delete_devices, get_device, get_devices, update_device,
	},
};
use tuwunel_core::{Err, Result, debug, err, utils::string::to_small_string};

use crate::{Ruma, router::auth_uiaa};

/// # `GET /_matrix/client/r0/devices`
///
/// Get metadata on all devices of the sender user.
pub(crate) async fn get_devices_route(
	State(services): State<crate::State>,
	body: Ruma<get_devices::v3::Request>,
) -> Result<get_devices::v3::Response> {
	let devices: Vec<device::Device> = services
		.users
		.all_devices_metadata(body.sender_user())
		.collect()
		.await;

	Ok(get_devices::v3::Response { devices })
}

/// # `GET /_matrix/client/r0/devices/{deviceId}`
///
/// Get metadata on a single device of the sender user.
pub(crate) async fn get_device_route(
	State(services): State<crate::State>,
	body: Ruma<get_device::v3::Request>,
) -> Result<get_device::v3::Response> {
	let device = services
		.users
		.get_device_metadata(body.sender_user(), &body.body.device_id)
		.await
		.map_err(|_| err!(Request(NotFound("Device not found."))))?;

	Ok(get_device::v3::Response { device })
}

/// # `PUT /_matrix/client/r0/devices/{deviceId}`
///
/// Updates the metadata on a given device of the sender user.
#[tracing::instrument(skip_all, fields(%client), name = "update_device")]
pub(crate) async fn update_device_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<update_device::v3::Request>,
) -> Result<update_device::v3::Response> {
	let sender_user = body.sender_user();
	let appservice = body.appservice_info.as_ref();

	match services
		.users
		.get_device_metadata(sender_user, &body.device_id)
		.await
	{
		| Ok(mut device) => {
			let notify = device.display_name != body.display_name;
			device.display_name.clone_from(&body.display_name);

			device
				.last_seen_ip
				.clone_from(&Some(to_small_string(client)));

			device
				.last_seen_ts
				.clone_from(&Some(MilliSecondsSinceUnixEpoch::now()));

			assert_eq!(device.device_id, body.device_id, "device_id mismatch");
			services
				.users
				.put_device_metadata(sender_user, notify, &device);

			Ok(update_device::v3::Response {})
		},
		| Err(_) => {
			let Some(appservice) = appservice else {
				return Err!(Request(NotFound("Device not found.")));
			};

			if !appservice.registration.device_management {
				return Err!(Request(NotFound("Device not found.")));
			}

			debug!(
				"Creating new device for {sender_user} from appservice {} as MSC4190 is enabled \
				 and device ID does not exist",
				appservice.registration.id
			);

			services
				.users
				.create_device(
					sender_user,
					None,
					(Some(&appservice.registration.as_token), None),
					None,
					None,
					Some(client.to_string()),
				)
				.await?;

			return Ok(update_device::v3::Response {});
		},
	}
}

/// # `DELETE /_matrix/client/r0/devices/{deviceId}`
///
/// Deletes the given device.
///
/// - Requires UIAA to verify user password
/// - Invalidates access token
/// - Deletes device metadata (device id, device display name, last seen ip,
///   last seen ts)
/// - Forgets to-device events
/// - Triggers device list updates
pub(crate) async fn delete_device_route(
	State(services): State<crate::State>,
	body: Ruma<delete_device::v3::Request>,
) -> Result<delete_device::v3::Response> {
	let appservice = body.appservice_info.as_ref();

	if appservice.is_some_and(|appservice| appservice.registration.device_management) {
		let sender_user = body.sender_user();
		debug!(
			"Skipping UIAA for {sender_user} as this is from an appservice and MSC4190 is \
			 enabled"
		);
		services
			.users
			.remove_device(sender_user, &body.device_id)
			.await;

		return Ok(delete_device::v3::Response {});
	}

	let ref sender_user = auth_uiaa(&services, &body).await?;

	services
		.users
		.remove_device(sender_user, &body.device_id)
		.await;

	Ok(delete_device::v3::Response {})
}

/// # `POST /_matrix/client/v3/delete_devices`
///
/// Deletes the given list of devices.
///
/// - Requires UIAA to verify user password unless from an appservice with
///   MSC4190 enabled.
///
/// For each device:
/// - Invalidates access token
/// - Deletes device metadata (device id, device display name, last seen ip,
///   last seen ts)
/// - Forgets to-device events
/// - Triggers device list updates
pub(crate) async fn delete_devices_route(
	State(services): State<crate::State>,
	body: Ruma<delete_devices::v3::Request>,
) -> Result<delete_devices::v3::Response> {
	let appservice = body.appservice_info.as_ref();

	if appservice.is_some_and(|appservice| appservice.registration.device_management) {
		let sender_user = body.sender_user();
		debug!(
			"Skipping UIAA for {sender_user} as this is from an appservice and MSC4190 is \
			 enabled"
		);
		for device_id in &body.devices {
			services
				.users
				.remove_device(sender_user, device_id)
				.await;
		}

		return Ok(delete_devices::v3::Response {});
	}

	let ref sender_user = auth_uiaa(&services, &body).await?;

	for device_id in &body.devices {
		services
			.users
			.remove_device(sender_user, device_id)
			.await;
	}

	Ok(delete_devices::v3::Response {})
}
