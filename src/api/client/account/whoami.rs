use axum::extract::State;
use ruma::api::client::account::whoami;
use tuwunel_core::{Err, Result};

use crate::Ruma;

/// # `GET _matrix/client/r0/account/whoami`
///
/// Get `user_id` of the sender user.
///
/// Note: Also works for Application Services
pub(crate) async fn whoami_route(
	State(services): State<crate::State>,
	body: Ruma<whoami::v3::Request>,
) -> Result<whoami::v3::Response> {
	if !services.users.exists(body.sender_user()).await {
		return Err!(Request(Forbidden("User does not exist.")));
	}

	Ok(whoami::v3::Response {
		user_id: body.sender_user().to_owned(),
		device_id: body.sender_device.clone(),
		is_guest: body.appservice_info.is_none()
			&& services.users.is_guest(body.sender_user()).await,
	})
}
