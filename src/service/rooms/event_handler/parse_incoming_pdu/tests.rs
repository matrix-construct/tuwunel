use serde_json::value::RawValue;

use super::incoming_room_id;

#[test]
fn preliminary_room_matches_the_last_canonical_field() {
	let json = r#"{"room_id":"!first:example.org","room\u005fid":"!last:example.org"}"#;
	let raw: &RawValue = serde_json::from_str(json).expect("valid JSON");
	let room = incoming_room_id(raw).expect("valid room");
	assert_eq!(room.as_str(), "!last:example.org", "do not wait on a different room");
}
