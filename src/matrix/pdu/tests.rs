use ruma::{RoomVersionId, event_id, events::AnySyncMessageLikeEvent, serde::Raw, user_id};
use serde_json::{json, value::to_raw_value};

use super::{Count, Pdu, Unsigned};
use crate::Event;

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
fn sync_message_like_without_unsigned_borrows_pdu() {
	let pdu = message_pdu();
	let formatted = pdu.to_sync_message_like_without_unsigned();
	let formatted: serde_json::Value = serde_json::from_str(formatted.json().get())
		.expect("formatted event should be valid JSON");

	assert!(formatted.get("unsigned").is_none(), "formatted event retained unsigned data");
	assert_eq!(formatted["type"], "m.room.message", "formatted event changed type");
	assert!(pdu.unsigned.is_some(), "formatting mutated the source event");

	let mut custom = serde_json::to_value(&pdu).expect("serialize pdu");
	custom["type"] = json!("com.example.custom");
	let custom: Pdu = serde_json::from_value(custom).expect("valid custom pdu");
	let formatted = custom.to_sync_message_like_without_unsigned();
	let formatted: serde_json::Value = serde_json::from_str(formatted.json().get())
		.expect("formatted event should be valid JSON");

	assert_eq!(formatted["type"], "com.example.custom", "custom event type was lost");
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

fn member_pdu_with_raw_unsigned(unsigned: &str) -> Pdu {
	let mut pdu = member_pdu(&json!({}));
	pdu.unsigned =
		Some(Unsigned::from_json_string(unsigned.to_owned()).expect("valid raw unsigned object"));

	pdu
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

fn replacement_raw() -> Raw<AnySyncMessageLikeEvent> {
	to_raw_value(&json!({
		"type": "m.room.message",
		"content": {
			"msgtype": "m.text",
			"body": "* edited",
			"m.new_content": { "msgtype": "m.text", "body": "edited" },
			"m.relates_to": { "rel_type": "m.replace", "event_id": "$event:example.com" },
		},
		"event_id": "$edit:example.com",
		"sender": "@erased:example.com",
		"origin_server_ts": 1_838_188_001,
	}))
	.expect("valid replacement")
	.into()
}

#[test]
fn remove_replacement_bundle_round_trips_set() {
	let mut pdu = message_pdu();

	pdu.set_replacement_bundle(&replacement_raw())
		.expect("set failed");

	assert!(
		pdu.unsigned
			.as_ref()
			.expect("unsigned kept")
			.json()
			.get()
			.contains("\"m.replace\""),
		"set must fold the bundle"
	);

	pdu.remove_replacement_bundle()
		.expect("remove failed");

	let unsigned: serde_json::Value = serde_json::from_str(
		pdu.unsigned
			.as_ref()
			.expect("unsigned kept")
			.json()
			.get(),
	)
	.expect("valid unsigned");

	assert!(unsigned["m.relations"].get("m.replace").is_none());
	assert_eq!(unsigned["m.relations"]["m.thread"], json!({}));
	assert_eq!(unsigned["age"], 4);
}

#[test]
fn remove_replacement_bundle_drops_emptied_relations() {
	let mut pdu = member_pdu(&json!({
		"age": 4612,
		"m.relations": { "m.replace": { "event_id": "$edit:example.com" } },
	}));

	pdu.remove_replacement_bundle()
		.expect("remove failed");

	let unsigned = pdu.unsigned.as_ref().expect("unsigned kept");

	assert_eq!(unsigned.json().get(), r#"{"age":4612}"#);
}

#[test]
fn remove_replacement_bundle_omits_emptied_unsigned() {
	let mut pdu = member_pdu(&json!({
		"m.relations": { "m.replace": { "event_id": "$edit:example.com" } },
	}));

	pdu.remove_replacement_bundle()
		.expect("remove failed");

	assert!(pdu.unsigned.is_none());
}

#[test]
fn remove_replacement_bundle_ignores_nested_replace() {
	let mut pdu = member_pdu(&json!({
		"m.relations": {
			"m.thread": {
				"latest_event": {
					"unsigned": { "m.relations": { "m.replace": {} } },
				},
			},
		},
	}));

	let before = pdu
		.unsigned
		.as_ref()
		.expect("unsigned present")
		.json()
		.get()
		.to_owned();

	pdu.remove_replacement_bundle()
		.expect("remove failed");

	assert_eq!(
		pdu.unsigned
			.as_ref()
			.expect("unsigned kept")
			.json()
			.get(),
		before,
		"a nested bundle is not the top-level one"
	);
}

#[test]
fn set_reference_bundle_preserves_siblings_and_nested_reference() {
	let mut pdu = member_pdu(&json!({
		"age": 17,
		"m.relations": {
			"m.replace": { "event_id": "$edit:example.com" },
			"m.thread": {
				"latest_event": {
					"unsigned": {
						"m.relations": {
							"m.reference": {
								"chunk": [{ "event_id": "$nested:example.com" }],
							},
						},
					},
				},
			},
		},
	}));

	let event_ids = [
		event_id!("$one:example.com").to_owned(),
		event_id!("$two:example.com").to_owned(),
	];

	pdu.set_reference_bundle(&event_ids)
		.expect("set reference bundle");

	let unsigned: serde_json::Value = serde_json::from_str(
		pdu.unsigned
			.as_ref()
			.expect("unsigned kept")
			.json()
			.get(),
	)
	.expect("valid unsigned");

	assert_eq!(
		unsigned,
		json!({
			"age": 17,
			"m.relations": {
				"m.reference": {
					"chunk": [
						{ "event_id": "$one:example.com" },
						{ "event_id": "$two:example.com" },
					],
				},
				"m.replace": { "event_id": "$edit:example.com" },
				"m.thread": {
					"latest_event": {
						"unsigned": {
							"m.relations": {
								"m.reference": {
									"chunk": [{ "event_id": "$nested:example.com" }],
								},
							},
						},
					},
				},
			},
		}),
		"reference insertion changed sibling data",
	);
}

#[test]
fn remove_replacement_bundle_absent_unsigned_noop() {
	let mut pdu = member_pdu(&json!(null));

	pdu.remove_replacement_bundle()
		.expect("remove failed");

	assert!(pdu.unsigned.is_none());
}

#[test]
fn remove_thread_bundle_preserves_siblings() {
	let mut pdu = member_pdu(&json!({
		"age": 4612,
		"m.relations": {
			"m.replace": { "event_id": "$edit:example.com" },
			"m.thread": { "count": 3 },
		},
	}));

	pdu.remove_thread_bundle()
		.expect("thread bundle removal should succeed");

	let unsigned: serde_json::Value = serde_json::from_str(
		pdu.unsigned
			.as_ref()
			.expect("sibling unsigned data should remain")
			.json()
			.get(),
	)
	.expect("remaining unsigned data should be valid JSON");

	assert!(unsigned["m.relations"].get("m.thread").is_none());
	assert_eq!(unsigned["m.relations"]["m.replace"], json!({ "event_id": "$edit:example.com" }));
	assert_eq!(unsigned["age"], 4612);
}

#[test]
fn remove_thread_bundle_drops_emptied_relations() {
	let mut pdu = member_pdu(&json!({
		"age": 4612,
		"m.relations": { "m.thread": { "count": 3 } },
	}));

	pdu.remove_thread_bundle()
		.expect("thread bundle removal should succeed");

	let unsigned = pdu
		.unsigned
		.as_ref()
		.expect("sibling unsigned data should remain");

	assert_eq!(unsigned.json().get(), r#"{"age":4612}"#);
}

#[test]
fn remove_thread_bundle_omits_emptied_unsigned() {
	let mut pdu = member_pdu(&json!({
		"m.relations": { "m.thread": { "count": 3 } },
	}));

	pdu.remove_thread_bundle()
		.expect("thread bundle removal should succeed");

	assert!(pdu.unsigned.is_none());
}

#[test]
fn remove_thread_bundle_ignores_nested_thread() {
	let mut pdu = member_pdu(&json!({
		"m.relations": {
			"m.replace": {
				"latest_event": {
					"unsigned": { "m.relations": { "m.thread": {} } },
				},
			},
		},
	}));

	let before = pdu
		.unsigned
		.as_ref()
		.expect("thread unsigned data should be present")
		.json()
		.get()
		.to_owned();

	pdu.remove_thread_bundle()
		.expect("thread bundle removal should succeed");

	assert_eq!(
		pdu.unsigned
			.as_ref()
			.expect("sibling unsigned data should remain")
			.json()
			.get(),
		before,
		"a nested bundle is not the top-level one",
	);
}

#[test]
fn remove_thread_bundle_decodes_escaped_relation_key() {
	let unsigned = r#"{
		"age": 4,
		"m.relations": {
			"m\u002ethread": { "count": 1 },
			"m.replace": { "event_id": "$edit:example.com" }
		}
	}"#;

	let mut pdu = member_pdu_with_raw_unsigned(unsigned);
	assert!(
		pdu.has_thread_bundle()
			.expect("presence check failed")
	);

	pdu.remove_thread_bundle().expect("remove failed");

	let unsigned: serde_json::Value = serde_json::from_str(
		pdu.unsigned
			.as_ref()
			.expect("unsigned kept")
			.json()
			.get(),
	)
	.expect("valid unsigned");

	assert_eq!(
		unsigned,
		json!({
			"age": 4,
			"m.relations": { "m.replace": { "event_id": "$edit:example.com" } },
		}),
	);
}

#[test]
fn remove_thread_bundle_absent_unsigned_noop() {
	let mut pdu = member_pdu(&json!(null));

	pdu.remove_thread_bundle()
		.expect("thread bundle removal should succeed");

	assert!(pdu.unsigned.is_none());
}

#[test]
fn thread_latest_event_rejects_malformed_identity() {
	let pdu = member_pdu(&json!({
		"m.relations": {
			"m.thread": {
				"latest_event": { "event_id": "$reply:example.com" },
			},
		},
	}));

	assert!(pdu.thread_latest_event().is_err(), "malformed identity was treated as absent");
}

#[test]
fn thread_latest_event_rejects_null_bundle_fields() {
	for unsigned in [
		json!({ "m.relations": { "m.thread": null } }),
		json!({ "m.relations": { "m.thread": { "latest_event": null } } }),
	] {
		let pdu = member_pdu(&unsigned);

		assert!(pdu.thread_latest_event().is_err(), "null bundle field was treated as absent");
	}
}

#[test]
fn remove_thread_latest_transaction_id_for_other_user() {
	let mut pdu = member_pdu(&json!({
		"age": 4612,
		"m.relations": {
			"m.replace": { "event_id": "$edit:example.com" },
			"m.thread": {
				"count": 1,
				"latest_event": {
					"content": { "body": "hello", "msgtype": "m.text" },
					"event_id": "$latest:example.com",
					"sender": "@bob:example.com",
					"unsigned": {
						"age": 23,
						"transaction_id": "secret",
					},
				},
			},
		},
	}));

	let mut expected: serde_json::Value = serde_json::from_str(
		pdu.unsigned
			.as_ref()
			.expect("unsigned present")
			.json()
			.get(),
	)
	.expect("valid unsigned");

	expected["m.relations"]["m.thread"]["latest_event"]["unsigned"]
		.as_object_mut()
		.expect("unsigned object")
		.remove("transaction_id");

	pdu.remove_thread_latest_transaction_id_unless_sender(user_id!("@alice:example.com"))
		.expect("remove failed");

	let unsigned: serde_json::Value = serde_json::from_str(
		pdu.unsigned
			.as_ref()
			.expect("unsigned kept")
			.json()
			.get(),
	)
	.expect("valid unsigned");

	assert_eq!(unsigned, expected, "rewrite changed fields beside the transaction ID");
}

#[test]
fn retain_thread_latest_transaction_id_for_sender() {
	let mut pdu = member_pdu(&json!({
		"m.relations": {
			"m.thread": {
				"latest_event": {
					"sender": "@bob:example.com",
					"unsigned": { "transaction_id": "secret" },
				},
			},
		},
	}));

	let before = pdu
		.unsigned
		.as_ref()
		.expect("unsigned present")
		.json()
		.get()
		.to_owned();

	pdu.remove_thread_latest_transaction_id_unless_sender(user_id!("@bob:example.com"))
		.expect("remove failed");

	let unsigned = pdu.unsigned.as_ref().expect("unsigned kept");

	assert_eq!(unsigned.json().get(), before, "own bundle was re-encoded");
}

#[test]
fn thread_latest_transaction_id_decodes_escaped_sender() {
	for (encoded, sender) in [
		(r"@\u0062ob:example.com", user_id!("@bob:example.com")),
		(r"@bo\\b:example.com", user_id!(r"@bo\b:example.com")),
	] {
		let unsigned = format!(
			r#"{{
				"age": 4612,
				"m.relations": {{
					"m.replace": {{ "event_id": "$edit:example.com" }},
					"m.thread": {{
						"count": 1,
						"latest_event": {{
							"sender": "{encoded}",
							"content": {{ "body": "hello" }},
							"unsigned": {{ "age": 23, "transaction_id": "secret" }}
						}}
					}}
				}}
			}}"#,
		);

		let mut pdu = member_pdu_with_raw_unsigned(&unsigned);

		pdu.remove_thread_latest_transaction_id_unless_sender(sender)
			.expect("retain for escaped sender failed");

		assert_eq!(
			pdu.unsigned
				.as_ref()
				.expect("unsigned kept")
				.json()
				.get(),
			unsigned,
			"own bundle was re-encoded",
		);

		let mut expected: serde_json::Value =
			serde_json::from_str(&unsigned).expect("valid unsigned");

		expected["m.relations"]["m.thread"]["latest_event"]["unsigned"]
			.as_object_mut()
			.expect("unsigned object")
			.remove("transaction_id");

		pdu.remove_thread_latest_transaction_id_unless_sender(user_id!("@alice:example.com"))
			.expect("remove for other user failed");

		let actual: serde_json::Value = serde_json::from_str(
			pdu.unsigned
				.as_ref()
				.expect("unsigned kept")
				.json()
				.get(),
		)
		.expect("valid unsigned");

		assert_eq!(actual, expected, "rewrite changed fields beside the transaction ID");
	}
}

#[test]
fn reject_thread_latest_transaction_id_without_sender() {
	let mut pdu = member_pdu(&json!({
		"m.relations": {
			"m.thread": {
				"latest_event": {
					"unsigned": { "transaction_id": "secret" },
				},
			},
		},
	}));

	let before = pdu
		.unsigned
		.as_ref()
		.expect("unsigned present")
		.json()
		.get()
		.to_owned();

	let result =
		pdu.remove_thread_latest_transaction_id_unless_sender(user_id!("@alice:example.com"));

	let unsigned = pdu.unsigned.as_ref().expect("unsigned kept");

	assert!(result.is_err(), "missing sender did not fail closed");
	assert_eq!(unsigned.json().get(), before, "failed rewrite mutated the bundle");
}

#[test]
fn reject_duplicate_thread_bundle_path() {
	let unsigned = r#"{
		"m.relations": {
			"m.thread": {
				"latest_event": {
					"sender": "@bob:example.com",
					"unsigned": { "transaction_id": "secret" }
				}
			}
		},
		"m.relations": {}
	}"#;

	let mut pdu = member_pdu_with_raw_unsigned(unsigned);

	let result =
		pdu.remove_thread_latest_transaction_id_unless_sender(user_id!("@alice:example.com"));

	assert!(result.is_err(), "duplicate path field did not fail closed");
	assert_eq!(
		pdu.unsigned
			.as_ref()
			.expect("unsigned kept")
			.json()
			.get(),
		unsigned,
		"failed rewrite mutated the bundle",
	);
}

#[test]
fn thread_bundle_fallback_preserves_unrelated_unsigned_data() {
	let unsigned = r#"{
		"age": 4,
		"m.relations": { "m.thread": { "count": 1 } },
		"m.relations": { "m.replace": { "event_id": "$edit:example.com" } }
	}"#;

	let mut pdu = member_pdu_with_raw_unsigned(unsigned);

	pdu.remove_thread_bundle()
		.expect("fallback removal failed");

	let unsigned: serde_json::Value = serde_json::from_str(
		pdu.unsigned
			.as_ref()
			.expect("unsigned kept")
			.json()
			.get(),
	)
	.expect("valid unsigned");

	assert_eq!(unsigned["age"], 4);
	assert_eq!(unsigned["m.relations"]["m.replace"]["event_id"], "$edit:example.com");
	assert!(unsigned["m.relations"].get("m.thread").is_none());
}

#[test]
fn remove_thread_latest_transaction_id_decodes_escaped_key() {
	let unsigned = r#"{
		"m.relations": {
			"m.thread": {
				"latest_event": {
					"sender": "@bob:example.com",
					"unsigned": { "transaction\u005fid": "secret" }
				}
			}
		}
	}"#;

	let mut pdu = member_pdu_with_raw_unsigned(unsigned);

	pdu.remove_thread_latest_transaction_id_unless_sender(user_id!("@alice:example.com"))
		.expect("remove failed");

	let unsigned: serde_json::Value = serde_json::from_str(
		pdu.unsigned
			.as_ref()
			.expect("unsigned kept")
			.json()
			.get(),
	)
	.expect("valid unsigned");

	let nested = &unsigned["m.relations"]["m.thread"]["latest_event"]["unsigned"];

	assert!(nested.get("transaction_id").is_none(), "escaped transaction ID survived");
}

#[test]
fn reject_duplicate_thread_bundle_sibling() {
	let unsigned = r#"{
		"m.relations": {
			"m.thread": {
				"latest_event": {
					"content": { "body": "one" },
					"content": { "body": "two" },
					"sender": "@bob:example.com",
					"unsigned": { "transaction_id": "secret" }
				}
			}
		}
	}"#;

	let mut pdu = member_pdu_with_raw_unsigned(unsigned);

	let result =
		pdu.remove_thread_latest_transaction_id_unless_sender(user_id!("@alice:example.com"));

	assert!(result.is_err(), "duplicate sibling field did not fail closed");
	assert_eq!(
		pdu.unsigned
			.as_ref()
			.expect("unsigned kept")
			.json()
			.get(),
		unsigned,
		"failed rewrite mutated the bundle",
	);
}
