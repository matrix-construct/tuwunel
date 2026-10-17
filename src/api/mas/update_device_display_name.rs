use axum::extract::State;
use synapse_admin_api::mas::update_device_display_name::{Request, Response};
use tuwunel_core::{Err, Result};

use super::{Mas, existing_user};
use crate::Ruma;

/// # `POST /_synapse/mas/update_device_display_name`
pub(crate) async fn update_device_display_name_route(
	_mas: Mas,
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let user_id = existing_user(services, &body.localpart).await?;

	if !services
		.users
		.device_exists(&user_id, &body.device_id)
		.await
	{
		return Err!(Request(NotFound("Device does not exist")));
	}

	services
		.users
		.rename_device(&user_id, &body.device_id, &body.display_name)
		.await?;

	Ok(Response::new())
}
