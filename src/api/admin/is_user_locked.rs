use axum::extract::State;
use ruma::api::client::admin::is_user_locked;
use tuwunel_core::Result;

use super::authorize;
use crate::Ruma;

/// # `GET /_matrix/client/v1/admin/lock/{userId}`
pub(crate) async fn is_user_locked_route(
	State(services): State<crate::State>,
	body: Ruma<is_user_locked::v1::Request>,
) -> Result<is_user_locked::v1::Response> {
	let user_id = &body.user_id;

	authorize(&services, body.sender_user(), user_id).await?;

	Ok(is_user_locked::v1::Response::new(services.users.is_locked(user_id).await))
}
