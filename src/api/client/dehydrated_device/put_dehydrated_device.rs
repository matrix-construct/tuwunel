use axum::extract::State;
use ruma::api::client::dehydrated_device::put_dehydrated_device::unstable as put_dehydrated_device;
use tuwunel_core::Result;

use crate::{ClientIp, Ruma};

/// # `PUT /_matrix/client/../dehydrated_device`
///
/// Creates or overwrites the user's dehydrated device.
#[tracing::instrument(skip_all, fields(%client))]
pub(crate) async fn put_dehydrated_device_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<put_dehydrated_device::Request>,
) -> Result<put_dehydrated_device::Response> {
	let sender_user = body
		.sender_user
		.as_deref()
		.expect("AccessToken authentication required");

	let device_id = body.body.device_id.clone();

	services
		.users
		.set_dehydrated_device(sender_user, body.body)
		.await?;

	Ok(put_dehydrated_device::Response { device_id })
}
