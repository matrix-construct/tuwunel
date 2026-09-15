use axum::extract::State;
use futures::{FutureExt, StreamExt, TryFutureExt, future::join4};
use ruma::{
	UserId,
	api::{
		client::device::Device,
		federation::{
			device::get_devices::{self, v1::UserDevice},
			keys::{claim_keys, get_keys},
		},
	},
	uint,
};
use tuwunel_core::{Err, Result, utils::future::TryExtExt};

use crate::{
	Ruma,
	client::{claim_keys_helper, get_keys_helper},
};

/// # `GET /_matrix/federation/v1/user/devices/{userId}`
///
/// Gets information on all devices of the user.
pub(crate) async fn get_devices_route(
	State(services): State<crate::State>,
	body: Ruma<get_devices::v1::Request>,
) -> Result<get_devices::v1::Response> {
	let user_id = &body.user_id;
	if !services.globals.user_is_local(user_id) {
		return Err!(Request(InvalidParam("Tried to access user from other server.")));
	}

	let allowed_signatures = |u: &UserId| u.server_name() == body.origin();

	let master_key = services
		.users
		.get_master_key(None, user_id, &allowed_signatures)
		.ok();

	let self_signing_key = services
		.users
		.get_self_signing_key(None, user_id, &allowed_signatures)
		.ok();

	let stream_id = services
		.users
		.get_devicelist_version(user_id)
		.map_ok(TryInto::try_into)
		.map_ok(Result::ok)
		.ok();

	let devices = services
		.users
		.all_devices_metadata(user_id)
		.filter_map(async |Device { device_id, display_name, .. }: Device| {
			let device_display_name = services
				.config
				.allow_device_name_federation
				.then_some(display_name)
				.flatten()
				.or_else(|| Some(device_id.as_str().into()));

			services
				.users
				.get_device_keys(user_id, &device_id)
				.map_ok(|keys| UserDevice {
					device_id: device_id.clone(),
					device_display_name,
					keys,
				})
				.map(Result::ok)
				.await
		})
		.collect::<Vec<_>>();

	let (stream_id, master_key, self_signing_key, devices) =
		join4(stream_id, master_key, self_signing_key, devices)
			.boxed()
			.await;

	Ok(get_devices::v1::Response {
		user_id: body.body.user_id,
		stream_id: stream_id.flatten().unwrap_or_else(|| uint!(0)),
		devices,
		self_signing_key,
		master_key,
	})
}

/// # `POST /_matrix/federation/v1/user/keys/query`
///
/// Gets devices and identity keys for the given users.
pub(crate) async fn get_keys_route(
	State(services): State<crate::State>,
	body: Ruma<get_keys::v1::Request>,
) -> Result<get_keys::v1::Response> {
	if body
		.device_keys
		.iter()
		.any(|(u, _)| !services.globals.user_is_local(u))
	{
		return Err!(Request(InvalidParam("User does not belong to this server.")));
	}

	let result = get_keys_helper(
		&services,
		None,
		&body.device_keys,
		|u| Some(u.server_name()) == body.origin.as_deref(),
		services.config.allow_device_name_federation,
	)
	.await?;

	Ok(get_keys::v1::Response {
		device_keys: result.device_keys,
		master_keys: result.master_keys,
		self_signing_keys: result.self_signing_keys,
	})
}

/// # `POST /_matrix/federation/v1/user/keys/claim`
///
/// Claims one-time keys.
pub(crate) async fn claim_keys_route(
	State(services): State<crate::State>,
	body: Ruma<claim_keys::v1::Request>,
) -> Result<claim_keys::v1::Response> {
	if body
		.one_time_keys
		.iter()
		.any(|(u, _)| !services.globals.user_is_local(u))
	{
		return Err!(Request(InvalidParam("Tried to access user from other server.")));
	}

	let result = claim_keys_helper(&services, &body.one_time_keys).await?;

	Ok(claim_keys::v1::Response { one_time_keys: result.one_time_keys })
}
