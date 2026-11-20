use axum::extract::State;
use ruma::{api::client::config::set_global_account_data, events::GlobalAccountDataEventType};
use tuwunel_core::{Err, Result};

use super::{assert_account_data_owner, set_account_data};
use crate::Ruma;

/// The pusher reads the whole list when checking a recipient for an event.
const MAX_IGNORED_USER_LIST_BYTES: usize = 1_048_576;

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

	if body.event_type == GlobalAccountDataEventType::IgnoredUserList
		&& body.data.json().get().len() > MAX_IGNORED_USER_LIST_BYTES
	{
		return Err!(Request(TooLarge("The ignored user list is too large.")));
	}

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
