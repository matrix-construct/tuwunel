use axum::extract::State;
use ruma::{
	api::client::config::get_global_account_data, events::AnyGlobalAccountDataEventContent,
	serde::Raw,
};
use serde::Deserialize;
use tuwunel_core::{Err, Result, err};

use super::{assert_account_data_owner, is_empty_content};
use crate::Ruma;

#[derive(Deserialize)]
struct ExtractGlobalEventContent {
	content: Raw<AnyGlobalAccountDataEventContent>,
}

/// # `GET /_matrix/client/r0/user/{userId}/account_data/{type}`
///
/// Gets some account data for the sender user.
pub(crate) async fn get_global_account_data_route(
	State(services): State<crate::State>,
	body: Ruma<get_global_account_data::v3::Request>,
) -> Result<get_global_account_data::v3::Response> {
	let sender_user = body.sender_user();

	assert_account_data_owner(
		sender_user,
		&body.user_id,
		body.appservice_info.as_ref(),
		"You cannot get account data of other users.",
	)?;

	let account_data: ExtractGlobalEventContent = services
		.account_data
		.get_global(&body.user_id, body.event_type.clone())
		.await
		.map_err(|_| err!(Request(NotFound("Data not found."))))?;

	if is_empty_content(&account_data.content) {
		return Err!(Request(NotFound("Data not found.")));
	}

	Ok(get_global_account_data::v3::Response { account_data: account_data.content })
}
