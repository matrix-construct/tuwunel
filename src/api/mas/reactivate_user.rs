use axum::extract::State;
use synapse_admin_api::mas::reactivate_user::{Request, Response};
use tuwunel_core::Result;
use tuwunel_service::users::PASSWORD_SENTINEL;

use super::{Mas, existing_user};
use crate::Ruma;

/// # `POST /_synapse/mas/reactivate_user`
pub(crate) async fn reactivate_user_route(
	_mas: Mas,
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let user_id = existing_user(services, &body.localpart).await?;

	services
		.users
		.set_password(&user_id, Some(PASSWORD_SENTINEL))
		.await?;

	Ok(Response::new())
}
