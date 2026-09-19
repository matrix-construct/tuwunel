use std::collections::HashMap;

use ruma::{OwnedEventId, events::TimelineEventType};
use serde_json::{
	from_str as from_json_str, to_string as to_json_string, to_vec as to_json_vec,
	value::RawValue as RawJsonValue,
};
use tuwunel_core::{
	itertools::iproduct,
	matrix::{Event, PduEvent},
};

use super::{
	super::super::events::is_power_event, alice, bob, event_id, is_power_event_id,
	to_init_pdu_event,
};

type Pdus = HashMap<OwnedEventId, PduEvent>;
type RawRows = HashMap<OwnedEventId, Vec<u8>>;

#[tokio::test]
async fn power_event_projection_matches_legacy_predicate() {
	let member_contents = [
		r#"{"membership":"leave"}"#,
		r#"{"membership":"ban"}"#,
		r#"{"membership":"join"}"#,
		r#"{"membership":"unknown"}"#,
		r#"{"membership":""}"#,
		"{}",
		r#"{"membership":null}"#,
		r#"{"membership":true}"#,
		r#"{"membership":7}"#,
		r#"{"membership":{}}"#,
		r#"{"membership":[]}"#,
		r#"["leave"]"#,
		r#"["ban"]"#,
		r#"["join"]"#,
		"[]",
		r#"["leave",0]"#,
		r#"{"membership":"leave","membership":"ban"}"#,
		r#"{"unknown":0,"unknown":1,"membership":"leave"}"#,
		r#"{"member\u0073hip":"le\u0061ve"}"#,
		"1e400",
		r#"{"membership":1e400}"#,
		r#"{"unknown":1e400,"membership":"leave"}"#,
		r#"{"membership":"ban","unknown":1e400}"#,
		r#"{"unknown":{"nested":[1e400]},"membership":"leave"}"#,
	];

	let member = TimelineEventType::RoomMember;
	for (i, content) in member_contents.into_iter().enumerate() {
		let id = format!("POWER_MEMBER_CONTENT_{i}");

		assert_power_projection_parity(&id, member.clone(), Some(bob().as_str()), content).await;
	}

	let leave = r#"{"membership":"leave"}"#;
	let member_state_keys = [None, Some(""), Some(alice().as_str()), Some(bob().as_str())];

	for (i, state_key) in member_state_keys.into_iter().enumerate() {
		let id = format!("POWER_MEMBER_STATE_KEY_{i}");

		assert_power_projection_parity(&id, member.clone(), state_key, leave).await;
	}

	let nonmember_types = [
		TimelineEventType::RoomCreate,
		TimelineEventType::RoomPowerLevels,
		TimelineEventType::RoomJoinRules,
		TimelineEventType::from("com.example.unknown"),
	];

	let nonmember_contents = [
		r#"{"membership":"leave"}"#,
		r#"{"membership":1e400}"#,
		r#"{"unknown":1e400}"#,
		"1e400",
		r#"["leave"]"#,
	];

	let nonmember_state_keys = [None, Some(""), Some("nonempty")];
	let nonmember_cases = iproduct!(nonmember_types, nonmember_contents, nonmember_state_keys);

	for (i, (kind, content, state_key)) in nonmember_cases.enumerate() {
		let id = format!("POWER_OTHER_{i}");

		assert_power_projection_parity(&id, kind, state_key, content).await;
	}

	let sender = to_json_string(alice().as_str()).unwrap();
	let state_key = to_json_string(bob().as_str()).unwrap();
	let escaped = format!(
		r#"{{"content":{{"member\u0073hip":"le\u0061ve"}},"state_key":{state_key},"sender":{sender},"t\u0079pe":"m.room.member","tail":{{"nested":[0]}}}}"#,
	);

	let null_state_key = format!(
		r#"{{"type":"m.room.member","sender":{sender},"state_key":null,"content":{{"membership":"leave"}},"tail":0}}"#,
	);

	let duplicate_membership = format!(
		r#"{{"type":"m.room.member","sender":{sender},"state_key":{state_key},"content":{{"member\u0073hip":"leave","membership":"ban"}},"tail":0}}"#,
	);

	let malformed_membership = format!(
		r#"{{"type":"m.room.member","sender":{sender},"state_key":{state_key},"content":{{"membership":1e400}},"tail":{{"nested":[0]}}}}"#,
	);

	let raw_rows = [
		(escaped, true),
		(null_state_key, true),
		(duplicate_membership, false),
		(malformed_membership, false),
	];

	for (i, (row, expected)) in raw_rows.into_iter().enumerate() {
		let event_id = event_id(&format!("POWER_RAW_ROW_{i}"));
		let rows: RawRows = [(event_id.clone(), row.into_bytes())].into();
		let decoded = is_power_event_id(&event_id, &rows).await.unwrap();

		assert_eq!(decoded, expected, "raw row projection {i}");
	}
}

async fn assert_power_projection_parity(
	id: &str,
	kind: TimelineEventType,
	state_key: Option<&str>,
	content: &str,
) {
	let content = from_json_str::<Box<RawJsonValue>>(content).unwrap();
	let event = to_init_pdu_event(id, alice(), kind, state_key, content);
	let event_id = event.event_id().to_owned();
	let expected = is_power_event(&event);
	let rows: RawRows = [(event_id.clone(), to_json_vec(&event).unwrap())].into();
	let pdus: Pdus = [(event_id.clone(), event)].into();
	let projected = is_power_event_id(&event_id, &pdus).await.unwrap();
	let decoded = is_power_event_id(&event_id, &rows).await.unwrap();

	assert_eq!(projected, expected, "PDU projection for {id}");
	assert_eq!(decoded, expected, "row projection for {id}");
}
