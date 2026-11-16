mod delete_global_account_data;
mod delete_room_account_data;
mod get_global_account_data;
mod get_room_account_data;
mod set_global_account_data;
mod set_room_account_data;
#[cfg(test)]
mod tests;

use ruma::{
	RoomId, UserId,
	events::{GlobalAccountDataEventType, RoomAccountDataEventType},
	serde::Raw,
};
use serde_json::{json, value::RawValue as RawJsonValue};
use tuwunel_core::{Err, Result, err, utils::json::is_empty_object};
use tuwunel_service::{Services, account_data::is_tombstone, appservice::RegistrationInfo};

pub(crate) use self::{
	delete_global_account_data::delete_global_account_data_route,
	delete_room_account_data::delete_room_account_data_route,
	get_global_account_data::get_global_account_data_route,
	get_room_account_data::get_room_account_data_route,
	set_global_account_data::set_global_account_data_route,
	set_room_account_data::set_room_account_data_route,
};

fn assert_account_data_owner(
	sender_user: &UserId,
	user_id: &UserId,
	appservice_info: Option<&RegistrationInfo>,
	message: &str,
) -> Result {
	if sender_user != user_id
		&& !appservice_info.is_some_and(|registration| registration.is_user_match(user_id))
	{
		return Err!(Request(Forbidden("{message}")));
	}

	Ok(())
}

async fn set_account_data(
	services: &Services,
	room_id: Option<&RoomId>,
	sender_user: &UserId,
	event_type_s: &str,
	data: &RawJsonValue,
) -> Result {
	validate_event_type(event_type_s)?;

	let data: serde_json::Value = serde_json::from_str(data.get())
		.map_err(|e| err!(Request(BadJson(warn!("Invalid JSON provided: {e}")))))?;

	services
		.account_data
		.update(
			room_id,
			sender_user,
			event_type_s.into(),
			&json!({
				"type": event_type_s,
				"content": data,
			}),
		)
		.await
}

fn validate_event_type(event_type: &str) -> Result {
	if event_type == RoomAccountDataEventType::FullyRead.to_cow_str() {
		return Err!(Request(BadJson(
			"This endpoint cannot be used for marking a room as fully read (setting \
			 m.fully_read)"
		)));
	}

	if event_type == GlobalAccountDataEventType::PushRules.to_cow_str() {
		return Err!(Request(BadJson(
			"This endpoint cannot be used for setting/configuring push rules."
		)));
	}

	Ok(())
}

/// MSC3391: tombstoned account data carries `content: {}`. Sync delta
/// surfaces the empty event so clients can apply the deletion; everywhere
/// else (GET, initial sync) treats it as not-present.
fn is_empty_content<T>(content: &Raw<T>) -> bool { is_empty_object(content.json()) }

/// Equivalent test against a stored account-data event (`{type, content}`)
/// rather than the bare `content` payload. Used by sync filters.
pub(crate) fn is_empty_account_data_event<T>(event: &Raw<T>) -> bool {
	is_tombstone(event.json().get().as_bytes())
}
