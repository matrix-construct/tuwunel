use axum::extract::State;
use synapse_admin_api::mas::query_user::{Request, Response};
use tuwunel_core::Result;

use super::{Mas, existing_user};
use crate::Ruma;

/// # `GET /_synapse/mas/query_user`
pub(crate) async fn query_user_route(
	_mas: Mas,
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let user_id = existing_user(services, &body.localpart).await?;

	let display_name = services.profile.displayname(&user_id).await.ok();
	let avatar_url = services
		.profile
		.avatar_url(&user_id)
		.await
		.ok()
		.map(|url| url.to_string());

	let is_suspended = services.users.is_suspended(&user_id).await;
	let is_deactivated = services.users.is_deactivated(&user_id).await?;

	let mut response = Response::new(user_id);
	response.display_name = display_name;
	response.avatar_url = avatar_url;
	response.is_suspended = is_suspended;
	response.is_deactivated = is_deactivated;

	Ok(response)
}
