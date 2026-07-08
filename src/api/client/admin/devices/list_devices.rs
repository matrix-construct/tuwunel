use axum::extract::State;
use futures::StreamExt;
use ruma::UInt;
use synapse_admin_api::devices::list_devices::v1 as list_devices;
use tuwunel_core::Result;

use super::{device_response, require_local_user};
use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v2/users/{user_id}/devices`
pub(crate) async fn admin_list_devices_route(
	State(services): State<crate::State>,
	body: Ruma<list_devices::Request>,
) -> Result<list_devices::Response> {
	require_admin(&services, body.sender_user()).await?;

	let user_id = &body.user_id;
	require_local_user(&services, user_id).await?;

	let dehydrated = services
		.users
		.get_dehydrated_device_id(user_id)
		.await
		.ok();

	let devices: Vec<_> = services
		.users
		.all_devices_metadata(user_id)
		.map(|device| device_response(device, user_id, dehydrated.as_deref()))
		.collect()
		.await;

	let total = UInt::try_from(devices.len()).unwrap_or(UInt::MAX);

	Ok(list_devices::Response { devices, total })
}
