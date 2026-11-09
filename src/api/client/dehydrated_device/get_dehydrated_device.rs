use axum::extract::State;
use ruma::api::client::dehydrated_device::get_dehydrated_device::unstable::{Request, Response};
use tuwunel_core::Result;

use crate::{ClientIp, Ruma};

/// # `GET /_matrix/client/../dehydrated_device`
///
/// Returns the user's dehydrated device.
///
/// The device data is opaque to the server and comes back as the client
/// stored it. A user without one gets `M_NOT_FOUND`.
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		%client,
	),
)]
pub(crate) async fn get_dehydrated_device_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<Request>,
) -> Result<Response> {
	let sender_user = body.sender_user();

	let device = services
		.users
		.get_dehydrated_device(sender_user)
		.await?;

	Ok(Response {
		device_id: device.device_id,
		device_data: device.device_data,
	})
}
