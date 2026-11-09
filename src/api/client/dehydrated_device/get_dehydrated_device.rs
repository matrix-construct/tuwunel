use axum::extract::State;
use ruma::api::client::dehydrated_device::get_dehydrated_device::unstable as get_dehydrated_device;
use tuwunel_core::Result;

use crate::{ClientIp, Ruma};

/// # `GET /_matrix/client/../dehydrated_device`
///
/// Gets the user's dehydrated device
#[tracing::instrument(skip_all, fields(%client))]
pub(crate) async fn get_dehydrated_device_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<get_dehydrated_device::Request>,
) -> Result<get_dehydrated_device::Response> {
	let sender_user = body.sender_user();

	let device = services
		.users
		.get_dehydrated_device(sender_user)
		.await?;

	Ok(get_dehydrated_device::Response {
		device_id: device.device_id,
		device_data: device.device_data,
	})
}
