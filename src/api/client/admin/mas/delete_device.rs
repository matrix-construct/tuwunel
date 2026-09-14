use axum::extract::State;
use synapse_admin_api::mas::delete_device::{Request, Response};
use tuwunel_core::Result;

use super::{Mas, existing_user};
use crate::Ruma;

/// # `POST /_synapse/mas/delete_device`
pub(crate) async fn delete_device_route(
	_mas: Mas,
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let user_id = existing_user(services, &body.localpart).await?;

	services
		.users
		.remove_device(&user_id, &body.device_id)
		.await;

	Ok(Response::new())
}
