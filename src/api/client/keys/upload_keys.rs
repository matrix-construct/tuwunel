use axum::extract::State;
use ruma::{
	DeviceId, UserId, api::client::keys::upload_keys, encryption::DeviceKeys, serde::Raw,
};
use tuwunel_core::{Err, Result, debug, err};
use tuwunel_service::Services;

use crate::Ruma;

/// # `POST /_matrix/client/r0/keys/upload`
///
/// Publish end-to-end encryption keys for the sender device.
///
/// - Adds one time keys
/// - If there are no device keys yet: Adds device keys (TODO: merge with
///   existing keys?)
pub(crate) async fn upload_keys_route(
	State(services): State<crate::State>,
	body: Ruma<upload_keys::v3::Request>,
) -> Result<upload_keys::v3::Response> {
	let sender_user = body.sender_user();
	let sender_device = body.sender_device()?;

	let one_time_keys = body
		.one_time_keys
		.iter()
		.take(services.config.one_time_key_limit)
		.map(|(id, val)| (id.as_ref(), val));

	services
		.users
		.add_one_time_keys(sender_user, sender_device, one_time_keys)
		.await?;

	if let Some(device_keys) = body.device_keys.as_ref() {
		store_device_keys(&services, sender_user, sender_device, device_keys).await?;
	}

	Ok(upload_keys::v3::Response {
		one_time_key_counts: services
			.users
			.count_one_time_keys(sender_user, sender_device)
			.await,
	})
}

async fn store_device_keys(
	services: &Services,
	sender_user: &UserId,
	sender_device: &DeviceId,
	device_keys: &Raw<DeviceKeys>,
) -> Result {
	let new_keys = device_keys.deserialize().map_err(|e| {
		err!(Request(BadJson(debug_warn!(
			?device_keys,
			"Invalid device keys JSON uploaded by client: {e}"
		))))
	})?;

	if new_keys.user_id != sender_user {
		return Err!(Request(Unknown(
			"User ID in keys uploaded does not match your own user ID"
		)));
	}
	if new_keys.device_id != sender_device {
		return Err!(Request(Unknown(
			"Device ID in keys uploaded does not match your own device ID"
		)));
	}

	// Workaround for a nheko bug which omits cross-signing signatures when
	// re-uploading the same DeviceKeys: ignore an exact-copy re-upload so the
	// existing signatures are preserved.
	let unchanged = services
		.users
		.get_device_keys(sender_user, sender_device)
		.await
		.and_then(|keys| keys.deserialize().map_err(Into::into))
		.is_ok_and(|existing| existing.keys == new_keys.keys);

	if unchanged {
		debug!(
			?sender_user,
			?sender_device,
			?device_keys,
			"Ignoring user uploaded keys as they are an exact copy already in the database"
		);

		return Ok(());
	}

	services
		.users
		.add_device_keys(sender_user, sender_device, device_keys)
		.await;

	Ok(())
}
