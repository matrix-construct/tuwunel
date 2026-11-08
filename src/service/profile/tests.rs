#[path = "tests/publication.rs"]
mod publication;
mod remote;

use ruma::{
	CanonicalJsonObject, MxcUri, api::error::ErrorKind,
	events::room::member::RoomMemberEventContent, profile::ProfileFieldName, room_id, user_id,
};
use serde_json::{Value, json};
use tuwunel_core::{Error, http::StatusCode};
use tuwunel_database::{deserialize_from_slice, serialize_to_vec};

use super::{
	MAX_AVATAR_URL_LENGTH, MAX_DISPLAYNAME_LENGTH, MAX_STATUS_EMOJI_LENGTH,
	MAX_STATUS_TEXT_LENGTH, MAX_TIMEZONE_LENGTH, ProfileValue, changed_writes,
	check_member_content, check_profile_value, overlay_holds, overlay_member_content,
};

const STATUS: &str = "org.matrix.msc4426.status";
const CALL: &str = "org.matrix.msc4426.call";
const TZ: &str = "m.tz";
const TZ_UNSTABLE: &str = "us.cloke.msc4175.tz";
const DISPLAYNAME: &str = "displayname";
const AVATAR_URL: &str = "avatar_url";

// Three bytes, so a short run of it overruns the avatar byte budget.
const EURO: &str = "€";

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

#[test]
fn displayname_accepts_its_budget_exactly() {
	let name = "x".repeat(MAX_DISPLAYNAME_LENGTH);

	check_profile_value(DISPLAYNAME, &json!(name)).unwrap();
}

#[test]
fn displayname_past_its_budget_is_bad_json() {
	let name = "x".repeat(MAX_DISPLAYNAME_LENGTH + 1);

	assert!(bad_json(&check_profile_value(DISPLAYNAME, &json!(name)).unwrap_err()));
}

// 800 bytes is over 256.
#[test]
fn displayname_budget_counts_characters_not_bytes() {
	let name = PALM.repeat(200);

	assert_eq!(name.len(), 800);
	check_profile_value(DISPLAYNAME, &json!(name)).unwrap();
}

#[test]
fn displayname_of_wide_characters_past_its_budget_is_bad_json() {
	let name = PALM.repeat(MAX_DISPLAYNAME_LENGTH + 1);

	assert!(bad_json(&check_profile_value(DISPLAYNAME, &json!(name)).unwrap_err()));
}

#[test]
fn displayname_empty_or_null_is_accepted() {
	check_profile_value(DISPLAYNAME, &json!("")).unwrap();
	check_profile_value(DISPLAYNAME, &json!(null)).unwrap();
}

#[test]
fn avatar_url_accepts_its_budget_exactly() {
	let prefix = "mxc://localhost/";
	let url = format!("{prefix}{}", "a".repeat(MAX_AVATAR_URL_LENGTH - prefix.len()));

	assert_eq!(url.len(), MAX_AVATAR_URL_LENGTH);
	check_profile_value(AVATAR_URL, &json!(url)).unwrap();
}

#[test]
fn avatar_url_past_its_budget_is_bad_json() {
	let url = "x".repeat(MAX_AVATAR_URL_LENGTH + 1);

	assert!(bad_json(&check_profile_value(AVATAR_URL, &json!(url)).unwrap_err()));
}

// 1002 bytes is over 1000.
#[test]
fn avatar_url_budget_counts_bytes_not_characters() {
	let url = EURO.repeat(334);

	assert_eq!(url.len(), 1002);
	assert!(bad_json(&check_profile_value(AVATAR_URL, &json!(url)).unwrap_err()));
}

#[test]
fn avatar_url_empty_or_null_is_accepted() {
	check_profile_value(AVATAR_URL, &json!("")).unwrap();
	check_profile_value(AVATAR_URL, &json!(null)).unwrap();
}

#[test]
fn displayname_as_a_non_string_is_bad_json() {
	assert!(bad_json(&check_profile_value(DISPLAYNAME, &json!({"x": "y"})).unwrap_err()));
	assert!(bad_json(&check_profile_value(DISPLAYNAME, &json!(42)).unwrap_err()));
}

#[test]
fn avatar_url_as_a_non_string_is_bad_json() {
	assert!(bad_json(&check_profile_value(AVATAR_URL, &json!({"x": "y"})).unwrap_err()));
	assert!(bad_json(&check_profile_value(AVATAR_URL, &json!(42)).unwrap_err()));
}

#[test]
fn field_budgets_are_pinned() {
	assert_eq!(MAX_DISPLAYNAME_LENGTH, 256);
	assert_eq!(MAX_AVATAR_URL_LENGTH, 1000);
}

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

#[test]
fn overlay_keeps_keys_the_model_does_not_know() {
	let member = json!({
		"membership": "join",
		"displayname": "old",
		"com.example.flag": {"a": [1]},
	});

	let expected = json!({
		"membership": "join",
		"displayname": "new",
		"com.example.flag": {"a": [1]},
	});

	let content = overlay(member, &[set(ProfileFieldName::DisplayName, "new")]);

	assert_eq!(content, expected);
}

#[test]
fn overlay_drops_reason_and_authorising_user() {
	let member = json!({
		"membership": "join",
		"displayname": "old",
		"reason": "why",
		"join_authorised_via_users_server": "@sponsor:example.com",
	});

	let content = overlay(member, &[set(ProfileFieldName::DisplayName, "new")]);

	assert_eq!(content, json!({"membership": "join", "displayname": "new"}));
}

#[test]
fn overlay_removes_a_cleared_field() {
	let member = json!({
		"membership": "join",
		"displayname": "old",
		"avatar_url": "mxc://example.com/abc",
	});

	let content = overlay(member, &[clear(ProfileFieldName::DisplayName)]);

	assert_eq!(content, json!({"membership": "join", "avatar_url": "mxc://example.com/abc"}));
}

#[test]
fn overlay_copies_an_unassigned_empty_avatar() { copies_an_unassigned_avatar(&json!("")); }

#[test]
fn overlay_copies_an_unassigned_null_avatar() { copies_an_unassigned_avatar(&json!(null)); }

#[test]
fn changed_writes_skips_a_restored_value() {
	let member = typed_member(json!({"membership": "join", "displayname": "same"}));

	assert_eq!(global_count(&member, &[set(ProfileFieldName::DisplayName, "same")]), 0);
}

#[test]
fn changed_writes_skips_clearing_an_absent_field() {
	let member = typed_member(json!({"membership": "join"}));

	assert_eq!(global_count(&member, &[clear(ProfileFieldName::DisplayName)]), 0);
}

#[test]
fn changed_writes_reports_a_real_change() {
	let member = typed_member(json!({"membership": "join", "displayname": "old"}));

	assert_eq!(global_count(&member, &[set(ProfileFieldName::DisplayName, "new")]), 1);
}

#[test]
fn changed_writes_reports_clearing_a_present_field() {
	let member = typed_member(json!({"membership": "join", "displayname": "old"}));

	assert_eq!(global_count(&member, &[clear(ProfileFieldName::DisplayName)]), 1);
}

#[test]
fn changed_writes_clearing_a_stored_empty_avatar_skips() {
	let member = typed_member(json!({"membership": "join", "avatar_url": ""}));

	assert_eq!(global_count(&member, &[clear(ProfileFieldName::AvatarUrl)]), 0);
}

#[test]
fn changed_writes_leaves_a_room_override_under_unchanged() {
	let member = typed_member(json!({"membership": "join", "displayname": "per-room"}));

	assert_eq!(override_count(&member, &[set(ProfileFieldName::DisplayName, "new")]), 0);
}

#[test]
fn changed_writes_follows_an_unchanged_match() {
	let member = typed_member(json!({"membership": "join", "displayname": "global"}));

	assert_eq!(override_count(&member, &[set(ProfileFieldName::DisplayName, "new")]), 1);
}

#[test]
fn changed_writes_reports_a_new_avatar() {
	let member =
		typed_member(json!({"membership": "join", "avatar_url": "mxc://example.com/abc"}));

	let writes =
		global_count(&member, &[set(ProfileFieldName::AvatarUrl, "mxc://example.com/new")]);

	assert_eq!(writes, 1);
}

#[test]
fn changed_writes_reports_setting_an_absent_avatar() {
	let member = typed_member(json!({"membership": "join"}));

	let writes =
		global_count(&member, &[set(ProfileFieldName::AvatarUrl, "mxc://example.com/new")]);

	assert_eq!(writes, 1);
}

#[test]
fn changed_writes_reports_clearing_a_stored_avatar() {
	let member =
		typed_member(json!({"membership": "join", "avatar_url": "mxc://example.com/abc"}));

	assert_eq!(global_count(&member, &[set(ProfileFieldName::AvatarUrl, "")]), 1);
}

#[test]
fn changed_writes_follows_an_unchanged_avatar_when_the_global_is_empty() {
	let member = typed_member(json!({"membership": "join"}));
	let new_avatar = [set(ProfileFieldName::AvatarUrl, "mxc://example.com/new")];

	assert_eq!(empty_global_count(&member, &new_avatar), 1);
}

#[test]
fn overlay_holds_rejects_an_absent_avatar_for_a_set_write() {
	let member = typed_member(json!({"membership": "join"}));

	assert!(!holds(&member, &[set(ProfileFieldName::AvatarUrl, "mxc://example.com/new")]));
}

#[test]
fn overlay_holds_accepts_an_absent_avatar_for_an_empty_write() {
	let member = typed_member(json!({"membership": "join"}));

	assert!(holds(&member, &[set(ProfileFieldName::AvatarUrl, "")]));
}

#[test]
fn overlay_holds_accepts_a_matching_avatar() {
	let member =
		typed_member(json!({"membership": "join", "avatar_url": "mxc://example.com/new"}));

	assert!(holds(&member, &[set(ProfileFieldName::AvatarUrl, "mxc://example.com/new")]));
}

#[test]
fn overlay_holds_accepts_a_matching_displayname() {
	let member = typed_member(json!({"membership": "join", "displayname": "new"}));

	assert!(holds(&member, &[set(ProfileFieldName::DisplayName, "new")]));
}

#[test]
fn overlay_holds_rejects_a_mismatched_displayname() {
	let member = typed_member(json!({"membership": "join", "displayname": "other"}));

	assert!(!holds(&member, &[set(ProfileFieldName::DisplayName, "new")]));
}

#[test]
fn overlay_holds_rejects_a_mismatched_avatar() {
	let member =
		typed_member(json!({"membership": "join", "avatar_url": "mxc://example.com/other"}));

	assert!(!holds(&member, &[set(ProfileFieldName::AvatarUrl, "mxc://example.com/new")]));
}

#[test]
fn overlay_holds_rejects_a_non_join_membership() {
	let member = typed_member(json!({"membership": "leave", "displayname": "new"}));

	assert!(!holds(&member, &[set(ProfileFieldName::DisplayName, "new")]));
}

fn copies_an_unassigned_avatar(avatar: &Value) {
	let member = json!({"membership": "join", "displayname": "old", "avatar_url": avatar});

	let content = overlay(member, &[set(ProfileFieldName::DisplayName, "new")]);

	let expected = json!({"membership": "join", "displayname": "new", "avatar_url": avatar});

	assert_eq!(content, expected);
}

fn overlay(content: Value, writes: &[ProfileValue]) -> Value {
	let content = canonical(content);
	let overlaid = overlay_member_content(content, &refs(writes)).expect("overlay");

	serde_json::to_value(overlaid).expect("overlaid json")
}

fn canonical(content: Value) -> CanonicalJsonObject {
	serde_json::from_value(content).expect("canonical content")
}

fn holds(member: &RoomMemberEventContent, writes: &[ProfileValue]) -> bool {
	overlay_holds(member, &refs(writes))
}

fn refs(writes: &[ProfileValue]) -> Vec<&ProfileValue> { writes.iter().collect() }

fn global_count(member: &RoomMemberEventContent, writes: &[ProfileValue]) -> usize {
	changed_writes(member, writes, false, None, None).len()
}

fn override_count(member: &RoomMemberEventContent, writes: &[ProfileValue]) -> usize {
	changed_writes(member, writes, true, Some("global"), None).len()
}

fn empty_global_count(member: &RoomMemberEventContent, writes: &[ProfileValue]) -> usize {
	changed_writes(member, writes, true, None, Some(<&MxcUri>::from(""))).len()
}

fn set(name: ProfileFieldName, value: &str) -> ProfileValue { (name, Some(json!(value))) }

fn clear(name: ProfileFieldName) -> ProfileValue { (name, None) }

fn typed_member(content: Value) -> RoomMemberEventContent {
	serde_json::from_value(content).expect("member content")
}

#[test]
fn member_content_cap_counts_bytes() {
	// Each "é" is two bytes, so 2048 of them fill the 4096-byte cap exactly. A
	// character count would admit the one extra byte as well.
	let at_cap = "é".repeat(2048);
	let over_cap = format!("{at_cap}a");

	check_member_content(4096, at_cap.len(), None, None).expect("at cap");

	let error = check_member_content(4096, over_cap.len(), None, None).expect_err("over cap");

	assert!(bad_json(&error));
}

#[test]
fn member_displayname_cap_counts_characters() {
	let longest = "é".repeat(MAX_DISPLAYNAME_LENGTH);
	let too_long = "é".repeat(MAX_DISPLAYNAME_LENGTH + 1);

	check_member_content(usize::MAX, 0, Some(&longest), None).expect("longest name");

	let error = check_member_content(usize::MAX, 0, Some(&too_long), None).expect_err("too long");

	assert!(bad_json(&error));
}

#[test]
fn member_avatar_cap_counts_bytes() {
	let longest = format!("mxc://localhost/{}", "a".repeat(MAX_AVATAR_URL_LENGTH - 16));
	let too_long = format!("{longest}a");

	check_member_content(usize::MAX, 0, None, Some(&longest)).expect("longest avatar");

	let error = check_member_content(usize::MAX, 0, None, Some(&too_long)).expect_err("too long");

	assert!(bad_json(&error));
}

#[test]
fn member_avatar_must_be_an_mxc_uri() {
	for valid in ["mxc://localhost/abc", "mxc://1.2.3.4:8448/abc", "mxc://[2001:db8::1]:8448/abc"]
	{
		check_member_content(usize::MAX, 0, None, Some(valid))
			.unwrap_or_else(|e| panic!("{valid} refused: {e}"));
	}

	for invalid in [
		"mxc://b\u{fc}cher.example/abc",
		"mxc://localhost/caf\u{e9}",
		"mxc://localhost/a%20b",
		"mxc://localhost/",
		"https://localhost/abc",
	] {
		let error = check_member_content(usize::MAX, 0, None, Some(invalid)).expect_err(invalid);

		assert!(bad_json(&error), "{invalid}");
	}
}

fn bad_json(error: &Error) -> bool {
	matches!(error.kind(), ErrorKind::BadJson) && error.status_code() == StatusCode::BAD_REQUEST
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
