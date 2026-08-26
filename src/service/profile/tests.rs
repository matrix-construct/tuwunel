use ruma::{api::error::ErrorKind, room_id, user_id};
use serde_json::json;
use tuwunel_core::{Error, http::StatusCode};
use tuwunel_database::{deserialize_from_slice, serialize_to_vec};

use super::{
	MAX_STATUS_EMOJI_LENGTH, MAX_STATUS_TEXT_LENGTH, MAX_TIMEZONE_LENGTH, check_profile_value,
};

const STATUS: &str = "org.matrix.msc4426.status";
const CALL: &str = "org.matrix.msc4426.call";
const TZ: &str = "m.tz";
const TZ_UNSTABLE: &str = "us.cloke.msc4175.tz";

// Four bytes, so the emoji budget divides evenly into repeats of it.
const PALM: &str = "🌴";

#[test]
fn status_accepts_both_budgets_exactly() {
	let text = "a".repeat(MAX_STATUS_TEXT_LENGTH);
	let emoji = PALM.repeat(MAX_STATUS_EMOJI_LENGTH / PALM.len());
	let status = json!({ "text": text, "emoji": emoji });

	check_profile_value(STATUS, &status).unwrap();
}

// The 400 is mandated; the kind-derived table would otherwise promote it.
#[test]
fn status_text_past_its_budget_is_too_large() {
	let text = "a".repeat(MAX_STATUS_TEXT_LENGTH + 1);
	let status = json!({ "text": text, "emoji": PALM });

	assert!(too_large(&check_profile_value(STATUS, &status).unwrap_err()));
}

#[test]
fn status_emoji_past_its_budget_is_too_large() {
	let emoji = PALM.repeat(MAX_STATUS_EMOJI_LENGTH / PALM.len() + 1);
	let status = json!({ "text": "away", "emoji": emoji });

	assert!(too_large(&check_profile_value(STATUS, &status).unwrap_err()));
}

// Element Web clears a status by writing `null` rather than deleting the
// field, so the schema has to admit one.
#[test]
fn status_null_clears_the_field() { check_profile_value(STATUS, &json!(null)).unwrap(); }

#[test]
fn status_without_an_emoji_is_rejected() {
	let status = json!({ "text": "away" });

	assert_bad_json(&check_profile_value(STATUS, &status).unwrap_err());
}

#[test]
fn status_as_a_bare_string_is_rejected() {
	assert_bad_json(&check_profile_value(STATUS, &json!("away")).unwrap_err());
}

#[test]
fn call_accepts_an_empty_object() { check_profile_value(CALL, &json!({})).unwrap(); }

#[test]
fn call_accepts_a_join_timestamp() {
	let call = json!({ "call_joined_ts": 1_755_000_000 });

	check_profile_value(CALL, &call).unwrap();
}

#[test]
fn call_join_timestamp_must_be_a_number() {
	let call = json!({ "call_joined_ts": "recently" });

	assert_bad_json(&check_profile_value(CALL, &call).unwrap_err());
}

#[test]
fn unrecognized_field_carries_any_json() {
	check_profile_value("com.example.thing", &json!({ "any": [1, 2] })).unwrap();
}

#[test]
fn timezone_accepts_a_database_name() {
	check_profile_value(TZ, &json!("Europe/Paris")).unwrap();
	check_profile_value(TZ, &json!("UTC")).unwrap();
	check_profile_value(TZ, &json!("America/Argentina/Buenos_Aires")).unwrap();
	check_profile_value(TZ, &json!("Etc/GMT+12")).unwrap();
}

// Element Web publishes to the stable and unstable names on every update, so
// validating one without the other leaves a bypass.
#[test]
fn timezone_unstable_name_is_validated_too() {
	check_profile_value(TZ_UNSTABLE, &json!("Europe/Paris")).unwrap();

	assert_invalid_param(&check_profile_value(TZ_UNSTABLE, &json!("+05:30")).unwrap_err());
}

// The proposal's rejected alternative, which a client may still send.
#[test]
fn timezone_offset_is_rejected() {
	assert_invalid_param(&check_profile_value(TZ, &json!("+05:30")).unwrap_err());
	assert_invalid_param(&check_profile_value(TZ, &json!("0530")).unwrap_err());
}

// A Windows registry time zone id, which a client reading the platform's own
// identifier rather than an IANA one would send.
#[test]
fn timezone_display_name_is_rejected() {
	assert_invalid_param(&check_profile_value(TZ, &json!("Eastern Standard Time")).unwrap_err());
}

#[test]
fn timezone_malformed_path_is_rejected() {
	assert_invalid_param(&check_profile_value(TZ, &json!("")).unwrap_err());
	assert_invalid_param(&check_profile_value(TZ, &json!("America//New_York")).unwrap_err());
	assert_invalid_param(&check_profile_value(TZ, &json!("a/b/c/d")).unwrap_err());
}

#[test]
fn timezone_past_its_budget_is_rejected() {
	let name = "A".repeat(MAX_TIMEZONE_LENGTH + 1);

	assert_invalid_param(&check_profile_value(TZ, &json!(name)).unwrap_err());
}

// Both entry points type the stable name through ruma, which refuses a
// non-string first; only the unstable name reaches the check as raw JSON.
#[test]
fn timezone_as_a_non_string_is_rejected() {
	assert_invalid_param(&check_profile_value(TZ_UNSTABLE, &json!(3600)).unwrap_err());
}

#[test]
fn timezone_null_clears_the_field() { check_profile_value(TZ, &json!(null)).unwrap(); }

// Both readers decode the log's key and value back, and the codec checks for
// undecoded trailing bytes only in debug builds, so a drift in either shape
// misreads the log in release rather than failing loudly.
#[test]
fn change_log_record_round_trips() {
	let user_id = user_id!("@alice:example.com");
	let field = "org.matrix.msc4426.status";

	let key = serialize_to_vec((user_id.as_str(), 42_u64, field)).unwrap();

	let (prefix, count, changed_field): (&str, u64, &str) = deserialize_from_slice(&key).unwrap();

	assert_eq!(prefix, user_id.as_str());
	assert_eq!(count, 42);
	assert_eq!(changed_field, field);
}

// The same family carries a room-keyed copy of every change, and a room id is
// not a user id, so its prefix has to survive the same round trip.
#[test]
fn change_log_room_key_round_trips() {
	let room_id = room_id!("!room:example.com");

	let key = serialize_to_vec((room_id.as_str(), u64::MAX, "m.status")).unwrap();

	let (prefix, count, field): (&str, u64, &str) = deserialize_from_slice(&key).unwrap();

	assert_eq!(prefix, room_id.as_str());
	assert_eq!(count, u64::MAX);
	assert_eq!(field, "m.status");
}

fn too_large(error: &Error) -> bool {
	matches!(error.kind(), ErrorKind::TooLarge) && error.status_code() == StatusCode::BAD_REQUEST
}

fn assert_bad_json(error: &Error) {
	assert_eq!(error.kind(), ErrorKind::BadJson);
}

fn assert_invalid_param(error: &Error) {
	assert_eq!(error.kind(), ErrorKind::InvalidParam);
	assert_eq!(error.status_code(), StatusCode::BAD_REQUEST);
}
