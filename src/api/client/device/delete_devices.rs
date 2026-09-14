use axum::extract::State;
use ruma::api::client::device::delete_devices::{self, v3::Response};
use tuwunel_core::{
	Result, debug,
	utils::stream::{IterStream, ReadyExt, TryBroadbandExt},
};

use crate::{Ruma, router::auth_uiaa};

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
) -> Result<Response> {
	let appservice = body.appservice_info.as_ref();

	if appservice.is_some_and(|appservice| appservice.registration.device_management) {
		let sender_user = body.sender_user();
		debug!(
			"Skipping UIAA for {sender_user} as this is from an appservice and MSC4190 is \
			 enabled"
		);
		body.devices
			.iter()
			.try_stream()
			.broad_and_then(async |device_id| {
				services
					.users
					.remove_device(sender_user, device_id)
					.await
			})
			.ready_fold(Ok(()), Result::and)
			.await?;

		return Ok(Response {});
	}

	let ref sender_user = auth_uiaa(&services, &body).await?;

	body.devices
		.iter()
		.try_stream()
		.broad_and_then(async |device_id| {
			services
				.users
				.remove_device(sender_user, device_id)
				.await
		})
		.ready_fold(Ok(()), Result::and)
		.await?;

	Ok(Response {})
}
