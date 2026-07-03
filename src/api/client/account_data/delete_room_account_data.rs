use axum::extract::State;
use ruma::api::client::config::delete_room_account_data;
use tuwunel_core::Result;

use super::assert_account_data_owner;
use crate::Ruma;

/// # `DELETE /_matrix/client/unstable/org.matrix.msc3391/user/{userId}/rooms/{roomId}/account_data/{type}`
///
/// MSC3391: erase the named room account data type for the user.
pub(crate) async fn delete_room_account_data_route(
	State(services): State<crate::State>,
	body: Ruma<delete_room_account_data::unstable::Request>,
) -> Result<delete_room_account_data::unstable::Response> {
	let sender_user = body.sender_user();

	assert_account_data_owner(
		sender_user,
		&body.user_id,
		body.appservice_info.as_ref(),
		"You cannot delete account data for other users.",
	)?;

	services
		.account_data
		.delete(Some(&body.room_id), &body.user_id, body.event_type.clone())
		.await?;

	Ok(delete_room_account_data::unstable::Response {})
}
