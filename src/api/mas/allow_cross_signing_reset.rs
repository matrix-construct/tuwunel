use axum::extract::State;
use synapse_admin_api::mas::allow_cross_signing_reset::{Request, Response};
use tuwunel_core::Result;

use super::{Mas, existing_user};
use crate::Ruma;

/// # `POST /_synapse/mas/allow_cross_signing_reset`
pub(crate) async fn allow_cross_signing_reset_route(
	_mas: Mas,
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let user_id = existing_user(services, &body.localpart).await?;

	services
		.users
		.allow_cross_signing_replacement(&user_id);

	Ok(Response::new())
}
