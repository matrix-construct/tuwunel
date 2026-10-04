use axum::extract::State;
use ruma::api::client::admin::is_user_suspended;
use tuwunel_core::Result;

use super::authorize;
use crate::Ruma;

/// # `GET /_matrix/client/v1/admin/suspend/{userId}`
pub(crate) async fn is_user_suspended_route(
	State(services): State<crate::State>,
	body: Ruma<is_user_suspended::v1::Request>,
) -> Result<is_user_suspended::v1::Response> {
	let user_id = &body.user_id;

	authorize(&services, body.sender_user(), user_id).await?;

	Ok(is_user_suspended::v1::Response::new(services.users.is_suspended(user_id).await))
}
