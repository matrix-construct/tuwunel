use axum::extract::State;
use ruma::api::client::admin::suspend_user;
use tuwunel_core::Result;

use super::authorize;
use crate::Ruma;

/// # `PUT /_matrix/client/v1/admin/suspend/{userId}`
pub(crate) async fn suspend_user_route(
	State(services): State<crate::State>,
	body: Ruma<suspend_user::v1::Request>,
) -> Result<suspend_user::v1::Response> {
	let sender_user = body.sender_user();
	let user_id = &body.user_id;

	authorize(&services, sender_user, user_id).await?;

	if services.users.is_suspended(user_id).await == body.suspended {
		return Ok(suspend_user::v1::Response::new(body.suspended));
	}

	let action = match body.suspended {
		| true => {
			services.users.set_suspended(user_id, sender_user);
			"suspended"
		},
		| false => {
			services.users.clear_suspended(user_id);
			"unsuspended"
		},
	};

	services
		.admin
		.notify_loud(&format!("{user_id} has been {action} by {sender_user}."))
		.await;

	Ok(suspend_user::v1::Response::new(body.suspended))
}
