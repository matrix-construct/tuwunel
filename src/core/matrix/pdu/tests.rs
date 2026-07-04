use ruma::RoomVersionId;
use serde_json::json;

use super::{Count, Pdu};

fn message_pdu() -> Pdu {
	serde_json::from_value(json!({
		"type": "m.room.message",
		"content": { "msgtype": "m.text", "body": "secret" },
		"event_id": "$event:example.com",
		"room_id": "!room:example.com",
		"sender": "@erased:example.com",
		"prev_events": ["$prev:example.com"],
		"auth_events": ["$auth:example.com"],
		"origin_server_ts": 1_838_188_000,
		"depth": 12,
		"hashes": { "sha256": "thishashcoversallfieldsincasethisisredacted" },
		"unsigned": { "age": 4, "m.relations": { "m.thread": {} } },
	}))
	.expect("valid pdu")
}

#[test]
fn redacted_prunes_content_and_unsigned() {
	let pdu = message_pdu();

	let rules = RoomVersionId::V11.rules().expect("v11 rules");
	let redacted = pdu
		.redacted(&rules.redaction)
		.expect("redaction failed");

	assert_eq!(redacted.event_id, pdu.event_id);
	assert_eq!(redacted.sender, pdu.sender);
	assert!(redacted.unsigned.is_none(), "pruned form must carry no unsigned");
	assert!(!redacted.content.json().get().contains("secret"), "content must be pruned");
}

#[test]
fn redacted_keeps_member_membership() {
	let pdu: Pdu = serde_json::from_value(json!({
		"type": "m.room.member",
		"content": { "membership": "join", "displayname": "Erased", "reason": "hello" },
		"state_key": "@erased:example.com",
		"event_id": "$member:example.com",
		"room_id": "!room:example.com",
		"sender": "@erased:example.com",
		"prev_events": ["$prev:example.com"],
		"auth_events": ["$auth:example.com"],
		"origin_server_ts": 1_838_188_000,
		"depth": 12,
		"hashes": { "sha256": "thishashcoversallfieldsincasethisisredacted" },
	}))
	.expect("valid pdu");

	let rules = RoomVersionId::V11.rules().expect("v11 rules");
	let redacted = pdu
		.redacted(&rules.redaction)
		.expect("redaction failed");

	let content = redacted.content.json().get();

	assert!(content.contains("membership"), "membership survives redaction");
	assert!(!content.contains("displayname"), "displayname must be pruned");
	assert!(!content.contains("reason"), "reason must be pruned");
}

#[test]
fn backfilled_parse() {
	let count: Count = "-987654".parse().expect("parse() failed");
	let backfilled = matches!(count, Count::Backfilled(_));

	assert!(backfilled, "not backfilled variant");
}

#[test]
fn normal_parse() {
	let count: Count = "987654".parse().expect("parse() failed");
	let backfilled = matches!(count, Count::Backfilled(_));

	assert!(!backfilled, "backfilled variant");
}

fn member_pdu(unsigned: &serde_json::Value) -> Pdu {
	serde_json::from_value(json!({
		"type": "m.room.member",
		"content": { "membership": "join" },
		"event_id": "$member:example.com",
		"room_id": "!room:example.com",
		"sender": "@alice:example.com",
		"state_key": "@alice:example.com",
		"prev_events": ["$prev:example.com"],
		"auth_events": ["$auth:example.com"],
		"origin_server_ts": 1_838_188_000,
		"depth": 12,
		"hashes": { "sha256": "thishashcoversallfieldsincasethisisredacted" },
		"unsigned": unsigned,
	}))
	.expect("valid pdu")
}

#[test]
fn remove_prev_state_strips_pair() {
	let mut pdu = member_pdu(&json!({
		"age": 4612,
		"prev_content": { "membership": "invite" },
		"prev_sender": "@bob:example.com",
		"replaces_state": "$invite:example.com",
	}));

	pdu.remove_prev_state().expect("strip failed");

	let unsigned: serde_json::Value = serde_json::from_str(
		pdu.unsigned
			.as_ref()
			.expect("unsigned kept")
			.json()
			.get(),
	)
	.expect("valid unsigned");

	assert!(unsigned.get("prev_content").is_none());
	assert!(unsigned.get("prev_sender").is_none());
	assert_eq!(unsigned["replaces_state"], "$invite:example.com");
	assert_eq!(unsigned["age"], 4612);
}

#[test]
fn remove_prev_state_omits_emptied_unsigned() {
	let mut pdu = member_pdu(&json!({
		"prev_content": { "membership": "invite" },
		"prev_sender": "@bob:example.com",
	}));

	pdu.remove_prev_state().expect("strip failed");

	assert!(pdu.unsigned.is_none());
}

#[test]
fn remove_prev_state_omits_stored_empty_unsigned() {
	let mut pdu = member_pdu(&json!({}));

	pdu.remove_prev_state().expect("strip failed");

	assert!(pdu.unsigned.is_none());
}

#[test]
fn remove_prev_state_keeps_unrelated_unsigned() {
	let mut pdu = member_pdu(&json!({ "age": 4612 }));

	pdu.remove_prev_state().expect("strip failed");

	let unsigned = pdu.unsigned.as_ref().expect("unsigned kept");

	assert_eq!(unsigned.json().get(), r#"{"age":4612}"#);
}

#[test]
fn remove_prev_state_absent_unsigned_noop() {
	let mut pdu = member_pdu(&json!(null));

	pdu.remove_prev_state().expect("strip failed");

	assert!(pdu.unsigned.is_none());
}
