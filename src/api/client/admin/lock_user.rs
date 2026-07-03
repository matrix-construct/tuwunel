use axum::extract::State;
use ruma::api::client::admin::lock_user;
use tuwunel_core::Result;

use super::authorize;
use crate::Ruma;

/// # `PUT /_matrix/client/v1/admin/lock/{userId}`
pub(crate) async fn lock_user_route(
	State(services): State<crate::State>,
	body: Ruma<lock_user::v1::Request>,
) -> Result<lock_user::v1::Response> {
	let sender_user = body.sender_user();
	let user_id = &body.user_id;

	authorize(&services, sender_user, user_id).await?;

	if services.users.is_locked(user_id).await == body.locked {
		return Ok(lock_user::v1::Response::new(body.locked));
	}

	let action = match body.locked {
		| true => {
			services.users.set_locked(user_id, sender_user);
			"locked"
		},
		| false => {
			services.users.clear_locked(user_id);
			"unlocked"
		},
	};

	services
		.admin
		.notify_loud(&format!("{user_id} has been {action} by {sender_user}."))
		.await;

	Ok(lock_user::v1::Response::new(body.locked))
}
