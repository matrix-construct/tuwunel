use axum::extract::State;
use synapse_admin_api::mas::delete_user::{Request, Response};
use tuwunel_core::Result;
use tuwunel_service::users::DeactivationReason;

use super::{Mas, existing_user};
use crate::Ruma;

/// # `POST /_synapse/mas/delete_user`
pub(crate) async fn delete_user_route(
	_mas: Mas,
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let user_id = existing_user(services, &body.localpart).await?;

	services
		.deactivate
		.full_deactivate(&user_id, body.erase, DeactivationReason::Admin)
		.await?;

	Ok(Response::new())
}
