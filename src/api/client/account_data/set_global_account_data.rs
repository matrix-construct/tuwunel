use axum::extract::State;
use ruma::api::client::config::set_global_account_data;
use tuwunel_core::Result;

use super::{assert_account_data_owner, set_account_data};
use crate::Ruma;

/// # `PUT /_matrix/client/r0/user/{userId}/account_data/{type}`
///
/// Sets some account data for the sender user.
pub(crate) async fn set_global_account_data_route(
	State(services): State<crate::State>,
	body: Ruma<set_global_account_data::v3::Request>,
) -> Result<set_global_account_data::v3::Response> {
	let sender_user = body.sender_user();

	assert_account_data_owner(
		sender_user,
		&body.user_id,
		body.appservice_info.as_ref(),
		"You cannot set account data for other users.",
	)?;

	set_account_data(
		&services,
		None,
		&body.user_id,
		&body.event_type.to_string(),
		body.data.json(),
	)
	.await?;

	Ok(set_global_account_data::v3::Response {})
}
