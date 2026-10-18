use axum::extract::State;
use ruma::api::client::account::whoami;
use tuwunel_core::{Err, Result};

use crate::{Ruma, utils::sender_is_guest};

/// # `GET _matrix/client/r0/account/whoami`
///
/// Get `user_id` of the sender user.
///
/// Note: Also works for Application Services
pub(crate) async fn whoami_route(
	State(services): State<crate::State>,
	body: Ruma<whoami::v3::Request>,
) -> Result<whoami::v3::Response> {
	let sender_user = body.sender_user();

	// Skips the spec's SHOULD to verify registration, as appservice auth does.
	if body.appservice_info.is_none() && !services.users.exists(sender_user).await {
		return Err!(Request(Forbidden("User does not exist.")));
	}

	let is_guest = sender_is_guest(&services, &body).await;

	Ok(whoami::v3::Response {
		user_id: sender_user.to_owned(),
		device_id: body.sender_device.clone(),
		is_guest,
	})
}
