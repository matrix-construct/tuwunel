use axum::extract::State;
use ruma::{
	MilliSecondsSinceUnixEpoch,
	api::client::device::{
		Device, delete_device, get_device,
		update_device::{self, v3::Response as UpdateDeviceResponse},
	},
};
use tuwunel_core::{Err, Result, debug, err, utils::string::to_small_string};

use crate::{ClientIp, Ruma, router::auth_uiaa};

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
/// Updates the metadata on a given device of the sender user, or creates the
/// device when called by an appservice with MSC4190 enabled, returning 201 on
/// creation and 200 on update.
#[tracing::instrument(skip_all, fields(%client), name = "update_device")]
pub(crate) async fn update_device_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<update_device::v3::Request>,
) -> Result<UpdateDeviceResponse> {
	let sender_user = body.sender_user();
	let appservice = body.appservice_info.as_ref();
	let update = |device: Device| {
		// An omitted name leaves the device's own in place.
		let renamed = body
			.display_name
			.clone()
			.filter(|name| device.display_name.as_ref() != Some(name));

		let notify = renamed.is_some();
		let device = Device {
			display_name: renamed.or(device.display_name),
			last_seen_ip: Some(to_small_string(client)),
			last_seen_ts: Some(MilliSecondsSinceUnixEpoch::now()),
			..device
		};

		(device, notify)
	};

	match services
		.users
		.update_device_metadata(sender_user, &body.device_id, update)
		.await
	{
		| Ok(()) => Ok(UpdateDeviceResponse::new()),
		| Err(_) => {
			let Some(appservice) = appservice else {
				return Err!(Request(NotFound("Device not found.")));
			};

			if !appservice.registration.device_management {
				return Err!(Request(NotFound("Device management not enabled for appservice.")));
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
					Some(&body.device_id),
					(None, None),
					None,
					body.display_name.as_deref(),
					Some(client),
				)
				.await?;

			Ok(UpdateDeviceResponse::created())
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
