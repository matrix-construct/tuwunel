use axum::extract::State;
use ruma::api::client::dehydrated_device::delete_dehydrated_device::unstable as delete_dehydrated_device;
use tuwunel_core::Result;

use crate::{ClientIp, Ruma};

/// # `DELETE /_matrix/client/../dehydrated_device`
///
/// Deletes the user's dehydrated device without replacement.
#[tracing::instrument(skip_all, fields(%client))]
pub(crate) async fn delete_dehydrated_device_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<delete_dehydrated_device::Request>,
) -> Result<delete_dehydrated_device::Response> {
	let sender_user = body.sender_user();

	let device_id = services
		.users
		.get_dehydrated_device_id(sender_user)
		.await?;

	services
		.users
		.remove_device(sender_user, &device_id)
		.await;

	Ok(delete_dehydrated_device::Response { device_id })
}
