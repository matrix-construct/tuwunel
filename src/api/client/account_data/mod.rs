mod delete_global_account_data;
mod delete_room_account_data;
mod get_global_account_data;
mod get_room_account_data;
mod set_global_account_data;
mod set_room_account_data;

use ruma::{
	RoomId, UserId,
	events::{GlobalAccountDataEventType, RoomAccountDataEventType},
	serde::Raw,
};
use serde::Deserialize;
use serde_json::{Value as JsonValue, json, value::RawValue as RawJsonValue};
use tuwunel_core::{Err, Result, err};
use tuwunel_service::{Services, appservice::RegistrationInfo};

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
	if sender_user != user_id && appservice_info.is_none() {
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
	if event_type_s == RoomAccountDataEventType::FullyRead.to_cow_str() {
		return Err!(Request(BadJson(
			"This endpoint cannot be used for marking a room as fully read (setting \
			 m.fully_read)"
		)));
	}

	if event_type_s == GlobalAccountDataEventType::PushRules.to_cow_str() {
		return Err!(Request(BadJson(
			"This endpoint cannot be used for setting/configuring push rules."
		)));
	}

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

/// MSC3391: tombstoned account data carries `content: {}`. Sync delta
/// surfaces the empty event so clients can apply the deletion; everywhere
/// else (GET, initial sync) treats it as not-present.
fn is_empty_content<T>(content: &Raw<T>) -> bool { is_empty_object_json(content.json()) }

/// Equivalent test against a stored account-data event (`{type, content}`)
/// rather than the bare `content` payload. Used by sync filters.
pub(crate) fn is_empty_account_data_event<T>(event: &Raw<T>) -> bool {
	#[derive(Deserialize)]
	struct ContentOnly<'a> {
		#[serde(borrow)]
		content: &'a RawJsonValue,
	}

	serde_json::from_str::<ContentOnly<'_>>(event.json().get())
		.is_ok_and(|c| is_empty_object_json(c.content))
}

fn is_empty_object_json(s: &RawJsonValue) -> bool {
	serde_json::from_str::<JsonValue>(s.get())
		.ok()
		.and_then(|v| v.as_object().map(serde_json::Map::is_empty))
		.unwrap_or(false)
}
