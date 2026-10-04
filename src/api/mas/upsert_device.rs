use axum::extract::State;
use synapse_admin_api::mas::upsert_device::{Request, Response};
use tuwunel_core::Result;

use super::{Mas, existing_user};
use crate::Ruma;

/// # `POST /_synapse/mas/upsert_device`
pub(crate) async fn upsert_device_route(
	_mas: Mas,
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let user_id = existing_user(services, &body.localpart).await?;

	let created = if services
		.users
		.device_exists(&user_id, &body.device_id)
		.await
	{
		if let Some(display_name) = body.display_name.as_deref() {
			let mut device = services
				.users
				.get_device_metadata(&user_id, &body.device_id)
				.await?;

			device.display_name = Some(display_name.into());
			services
				.users
				.put_device_metadata(&user_id, true, &device)
				.await;
		}

		false
	} else {
		services
			.users
			.create_device(
				&user_id,
				Some(body.device_id.as_ref()),
				(None, None),
				None,
				body.display_name.as_deref(),
				None,
			)
			.await?;

		true
	};

	Ok(Response::new(created))
}
