use axum::extract::State;
use futures::StreamExt;
use ruma::api::client::account::change_password;
use tuwunel_core::{Result, info, utils::ReadyExt};

use crate::{ClientIp, Ruma, router::auth_uiaa};

/// # `POST /_matrix/client/r0/account/password`
///
/// Changes the password of this account.
///
/// - Requires UIAA to verify user password
/// - Changes the password of the sender user
/// - The password hash is calculated using argon2 with 32 character salt, the
///   plain password is
/// not saved
///
/// If logout_devices is true it does the following for each device except the
/// sender device:
/// - Invalidates access token
/// - Deletes device metadata (device id, device display name, last seen ip,
///   last seen ts)
/// - Forgets to-device events
/// - Triggers device list updates
#[tracing::instrument(skip_all, fields(%client), name = "change_password")]
pub(crate) async fn change_password_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<change_password::v3::Request>,
) -> Result<change_password::v3::Response> {
	let ref sender_user = auth_uiaa(&services, &body).await?;

	services
		.users
		.set_password(sender_user, Some(&body.new_password))
		.await?;

	if body.logout_devices {
		// Logout all devices except the current one
		services
			.users
			.all_device_ids(sender_user)
			.ready_filter(|&id| Some(id) != body.sender_device.as_deref())
			.for_each(|id| services.users.remove_device(sender_user, id))
			.await;
	}

	info!("User {sender_user} changed their password.");

	services
		.admin
		.notify(&format!("User {sender_user} changed their password."))
		.await;

	Ok(change_password::v3::Response {})
}
