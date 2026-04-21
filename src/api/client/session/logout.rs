use axum::extract::State;
use futures::StreamExt;
use ruma::api::client::session::{logout, logout_all};
use tuwunel_core::Result;

use crate::{Ruma, client_ip::ClientIp};

/// # `POST /_matrix/client/v3/logout`
///
/// Log out the current device.
///
/// - Invalidates access token
/// - Deletes device metadata (device id, device display name, last seen ip,
///   last seen ts)
/// - Forgets to-device events
/// - Triggers device list updates
#[tracing::instrument(skip_all, fields(%client), name = "logout")]
pub(crate) async fn logout_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<logout::v3::Request>,
) -> Result<logout::v3::Response> {
	services
		.users
		.remove_device(body.sender_user(), body.sender_device()?)
		.await;

	Ok(logout::v3::Response::new())
}

/// # `POST /_matrix/client/r0/logout/all`
///
/// Log out all devices of this user.
///
/// - Invalidates all access tokens
/// - Deletes all device metadata (device id, device display name, last seen ip,
///   last seen ts)
/// - Forgets all to-device events
/// - Triggers device list updates
///
/// Note: This is equivalent to calling [`GET
/// /_matrix/client/r0/logout`](fn.logout_route.html) from each device of this
/// user.
#[tracing::instrument(skip_all, fields(%client), name = "logout")]
pub(crate) async fn logout_all_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<logout_all::v3::Request>,
) -> Result<logout_all::v3::Response> {
	services
		.users
		.all_device_ids(body.sender_user())
		.for_each(|device_id| {
			services
				.users
				.remove_device(body.sender_user(), device_id)
		})
		.await;

	Ok(logout_all::v3::Response::new())
}
