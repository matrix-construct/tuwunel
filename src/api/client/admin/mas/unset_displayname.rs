use axum::extract::State;
use synapse_admin_api::mas::unset_displayname::{Request, Response};
use tuwunel_core::Result;

use super::{Mas, existing_user};
use crate::Ruma;

/// # `POST /_synapse/mas/unset_displayname`
pub(crate) async fn unset_displayname_route(
	_mas: Mas,
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let user_id = existing_user(services, &body.localpart).await?;

	services
		.profile
		.set_displayname(&user_id, None, None)
		.await?;

	Ok(Response::new())
}
