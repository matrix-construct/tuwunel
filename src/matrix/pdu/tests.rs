use ruma::{
	CanonicalJsonObject, RoomVersionId, event_id,
	events::{
		AnyMessageLikeEvent, AnySyncMessageLikeEvent, AnySyncTimelineEvent, AnyTimelineEvent,
	},
	serde::Raw,
	user_id,
};
use serde_json::{
	json,
	value::{RawValue as RawJsonValue, to_raw_value},
};

use super::{Content, Count, DELAY_ID, Pdu, Unsigned, into_outgoing_federation};
use crate::{
	Event,
	event::{STICKY_KEY, STICKY_TTL_KEY, Sticky},
};

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
fn room_formats_name_the_thread_latest_room() {
	let threaded = |latest: serde_json::Value| {
		message_pdu_with(&json!({
			"age": 4,
			"m.relations": {
				"m.replace": { "event_id": "$edit:example.com" },
				"m.thread": { "count": 1, "latest_event": latest },
			},
		}))
	};

	let pdu = threaded(json!({ "event_id": "$latest:example.com" }));
	let timeline: Raw<AnyTimelineEvent> = pdu.to_format();
	let message_like: Raw<AnyMessageLikeEvent> = pdu.to_format();
	let room_formats = [("timeline", timeline.json()), ("message-like", message_like.json())];

	for (format, event) in room_formats {
		let unsigned = formatted_unsigned(event);
		let relations = &unsigned["m.relations"];
		let latest = &relations["m.thread"]["latest_event"];

		assert_eq!(latest["room_id"], "!room:example.com", "{format}: the latest's room");
		assert_eq!(relations["m.replace"]["event_id"], "$edit:example.com", "{format}: the edit");
		assert_eq!(unsigned["age"], 4, "{format}: age");
	}

	let sync: Raw<AnySyncTimelineEvent> = pdu.to_format();
	let sync = formatted_unsigned(sync.json());
	let latest = &sync["m.relations"]["m.thread"]["latest_event"];

	assert_eq!(latest, &json!({ "event_id": "$latest:example.com" }), "sync named the room");

	let named = threaded(json!({ "event_id": "$latest:example.com", "room_id": "!other:a.b" }));
	let named: Raw<AnyTimelineEvent> = named.to_format();
	let named = formatted_unsigned(named.json());

	assert_eq!(named["m.relations"]["m.thread"]["latest_event"]["room_id"], "!other:a.b");

	let bare: Raw<AnyTimelineEvent> = message_pdu_with(&json!({ "age": 4 })).to_format();

	assert_eq!(formatted_unsigned(bare.json()), json!({ "age": 4 }));
}

fn message_pdu_with(unsigned: &serde_json::Value) -> Pdu {
	let unsigned =
		Unsigned::from_json_string(unsigned.to_string()).expect("valid raw unsigned object");

	Pdu {
		unsigned: Some(unsigned),
		..message_pdu()
	}
}

fn formatted_unsigned(event: &RawJsonValue) -> serde_json::Value {
	serde_json::from_str(event.get())
		.map(|mut event: serde_json::Value| event["unsigned"].take())
		.expect("formatted event should be valid JSON")
}

#[test]
fn relation_child_reads_any_rel_type_and_fails_closed() {
	let with_content = |content: &str| Pdu {
		content: Content::from_json_string(content.to_owned()).expect("valid raw content"),
		..message_pdu()
	};

	let children = [
		r#"{"m.relates_to": {"rel_type": "m.caption_fallback", "m.in_reply_to": {}}}"#,
		r#"{"m.relates_to": {"rel_type": "m.annotation", "event_id": "$target:example.com"}}"#,
		r#"{"m.relates_to": "$target:example.com"}"#,
		r#"{"m.relates_to": [null]}"#,
		r#"{"m.relates_to": {"rel_type": "m.thread", "rel_type": "m.thread"}}"#,
		r#"{"m.relates_to": {}, "m.relates_to": {}}"#,
	];

	let roots = [
		r#"{"body": "plain"}"#,
		r#"{"m.relates_to": null}"#,
		r#"{"m.relates_to": {"rel_type": null}}"#,
		r#"{"m.relates_to": {"m.in_reply_to": {"event_id": "$target:example.com"}}}"#,
	];

	for content in children {
		assert!(with_content(content).is_relation_child(), "not a relation child: {content}");
	}

	for content in roots {
		assert!(!with_content(content).is_relation_child(), "a relation child: {content}");
	}
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
fn thread_count_reads_only_the_thread_bundle_count() {
	let counted = member_pdu(&json!({
		"m.relations": {
			"m.replace": { "count": 7 },
			"m.thread": { "count": 3, "current_user_participated": false },
		},
	}));

	assert_eq!(
		counted
			.thread_count()
			.expect("valid thread count"),
		Some(3)
	);

	let threadless = member_pdu(&json!({ "m.relations": { "m.replace": { "count": 7 } } }));

	assert_eq!(
		threadless
			.thread_count()
			.expect("valid relations"),
		None
	);

	let malformed = member_pdu(&json!({ "m.relations": { "m.thread": { "count": "3" } } }));

	assert!(malformed.thread_count().is_err(), "a non-numeric count was read");
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

#[test]
fn private_removal_keeps_other_unsigned_fields() {
	let unsigned = json!({ "age": 1, "transaction_id": "secret", DELAY_ID: "syd_secret" });
	let pdu =
		member_pdu(&unsigned).without_private_unless_sender(Some(user_id!("@bob:example.com")));

	let unsigned = pdu.unsigned.expect("unsigned kept");

	assert_eq!(unsigned.json().get(), r#"{"age":1}"#, "removal changed the other fields");
}

#[test]
fn delay_id_is_served_to_its_sender_only() {
	let unsigned = json!({ "age": 1, DELAY_ID: "syd_scheduled" });
	let served = |requester| {
		let pdu = member_pdu(&unsigned).without_private_unless_sender(requester);
		let unsigned = pdu.unsigned.expect("unsigned kept");

		serde_json::from_str::<serde_json::Value>(unsigned.json().get()).expect("valid unsigned")
	};

	let sender = served(Some(user_id!("@alice:example.com")));
	let other = served(Some(user_id!("@bob:example.com")));
	let nobody = served(None);

	assert_eq!(sender, unsigned, "the sender lost the delay ID");
	assert_eq!(other, json!({ "age": 1 }), "another user saw the delay ID");
	assert_eq!(nobody, json!({ "age": 1 }), "no requester saw the delay ID");
}

#[test]
fn private_removal_drops_unparsable_unsigned() {
	let requester = Some(user_id!("@bob:example.com"));
	let unparsable = [
		r#""transaction_id""#,
		r#"["org.matrix.msc4140.delay_id", "syd_secret"]"#,
		r#"["transaction_id", "secret"]"#,
		r#"{ "\u0061ge": 1, "transaction_id": "secret" }"#,
	];

	for unsigned in unparsable {
		let stripped =
			member_pdu_with_raw_unsigned(unsigned).without_private_unless_sender(requester);

		assert!(stripped.unsigned.is_none(), "kept unparsable unsigned: {unsigned}");

		let served = member_pdu_with_raw_unsigned(unsigned).served_to(requester);
		let served = served.unsigned.expect("age added");
		let served: serde_json::Map<String, serde_json::Value> =
			serde_json::from_str(served.json().get()).expect("valid unsigned");

		assert_eq!(served.keys().collect::<Vec<_>>(), ["age"], "served part of: {unsigned}");
	}
}

#[test]
fn remove_aggregations_keeps_other_unsigned_fields() {
	let unsigned = json!({
		"prev_content": { "membership": "invite" },
		"transaction_id": "secret",
		"m.relations": {
			"m.replace": { "event_id": "$edit:example.com" },
			"m.thread": { "count": 1 },
		},
	});

	let pdu = member_pdu(&unsigned).without_aggregations();
	let unsigned = pdu.unsigned.expect("unsigned kept");
	let unsigned: serde_json::Value =
		serde_json::from_str(unsigned.json().get()).expect("valid unsigned");

	let expected = json!({
		"prev_content": { "membership": "invite" },
		"transaction_id": "secret",
	});

	assert_eq!(unsigned, expected, "removal changed the other fields");
}

#[test]
fn remove_aggregations_omits_emptied_unsigned() {
	let unsigned = json!({ "m.relations": { "m.thread": { "count": 1 } } });
	let pdu = member_pdu(&unsigned).without_aggregations();

	assert!(pdu.unsigned.is_none(), "kept an emptied unsigned object");
}

#[test]
fn remove_aggregations_decodes_escaped_key() {
	let unsigned = r#"{"m\u002erelations":{"m.thread":{"count":1}},"age":1}"#;
	let pdu = member_pdu_with_raw_unsigned(unsigned).without_aggregations();
	let unsigned = pdu.unsigned.expect("unsigned kept");

	assert_eq!(unsigned.json().get(), r#"{"age":1}"#, "escaped relations key survived");
}

#[test]
fn remove_aggregations_drops_unreadable_unsigned() {
	let unreadable = [
		r#""m.relations""#,
		r#"["m.relations", {}]"#,
		r#"{ "m.relations": {}, "m.relations": {} }"#,
	];

	for unsigned in unreadable {
		let pdu = member_pdu_with_raw_unsigned(unsigned).without_aggregations();

		assert!(pdu.unsigned.is_none(), "kept unreadable unsigned: {unsigned}");
	}
}

#[test]
fn remove_aggregations_leaves_absent_untouched() {
	let absent = Pdu { unsigned: None, ..member_pdu(&json!({})) };

	assert!(absent.without_aggregations().unsigned.is_none(), "added unsigned data");

	let untouched = [
		r#"{"prev_sender":"@bob:example.com","age":1}"#,
		r#"{"prev_content":{"topic":"m.relations"},"age":1}"#,
	];

	for unsigned in untouched {
		let pdu = member_pdu_with_raw_unsigned(unsigned).without_aggregations();
		let kept = pdu.unsigned.expect("unsigned kept");

		assert_eq!(kept.json().get(), unsigned, "rewrote unsigned without aggregations");
	}
}

#[test]
fn outgoing_federation_omits_unsigned() {
	let unsigned = json!({
		"transaction_id": "secret",
		"prev_content": { "msgtype": "m.text", "body": "earlier" },
		"m.relations": {
			"m.thread": {
				"count": 1,
				"latest_event": {
					"content": { "msgtype": "m.text", "body": "reply" },
					"unsigned": { "transaction_id": "nested" },
				},
			},
		},
	});

	let pdu = outgoing(&unsigned, &RoomVersionId::V11);

	assert!(!pdu.contains_key("unsigned"), "outgoing event kept unsigned data");
	assert!(pdu.contains_key("room_id"), "outgoing event lost its room ID");
	assert!(pdu.contains_key("hashes"), "outgoing event lost its hashes");
}

fn outgoing(unsigned: &serde_json::Value, room_version: &RoomVersionId) -> CanonicalJsonObject {
	let pdu = serde_json::from_value(json!({
		"type": "m.room.message",
		"content": { "msgtype": "m.text", "body": "root" },
		"event_id": "$root:example.com",
		"room_id": "!room:example.com",
		"sender": "@alice:example.com",
		"prev_events": ["$prev:example.com"],
		"auth_events": ["$auth:example.com"],
		"origin_server_ts": 1_838_188_000,
		"depth": 12,
		"hashes": { "sha256": "thishashcoversallfieldsincasethisisredacted" },
		"unsigned": unsigned,
	}))
	.expect("valid pdu object");

	into_outgoing_federation(pdu, room_version)
}

#[test]
fn outgoing_federation_omits_non_object_unsigned() {
	for unsigned in [json!("transaction_id"), json!(["transaction_id", "secret"])] {
		let pdu = outgoing(&unsigned, &RoomVersionId::V11);

		assert!(!pdu.contains_key("unsigned"), "outgoing event kept unsigned: {unsigned}");
	}
}

#[test]
fn outgoing_federation_without_rules_omits_unsigned() {
	let room_version = RoomVersionId::try_from("org.example.unknown").expect("custom version");
	let pdu = outgoing(&json!({ "transaction_id": "secret" }), &room_version);

	assert!(!pdu.contains_key("unsigned"), "unknown room version kept unsigned data");
	assert!(!pdu.contains_key("event_id"), "unknown room version kept the event ID");
}

const STICKY_START: u64 = 1_838_188_000;

#[test]
fn sticky_formats_carry_the_key_and_remaining_time() {
	let pdu = sticky_pdu(600_000);
	let now = STICKY_START + 100_000;
	let sync: Raw<AnySyncTimelineEvent> = pdu.to_format_at(now);
	let room: Raw<AnyTimelineEvent> = pdu.to_format_at(now);

	for (format, event) in [("sync", sync.json()), ("room", room.json())] {
		let event = formatted(event);

		assert_eq!(event[STICKY_KEY], json!({ "duration_ms": 600_000 }), "{format}: key");
		assert_eq!(event["unsigned"][STICKY_TTL_KEY], 500_000, "{format}: remaining time");
		assert_eq!(event["unsigned"]["age"], 4, "{format}: the rest of unsigned");
	}
}

fn sticky_pdu(duration_ms: u64) -> Pdu {
	Pdu {
		sticky: Some(Sticky { duration_ms }),
		..message_pdu()
	}
}

fn formatted(event: &RawJsonValue) -> serde_json::Value {
	serde_json::from_str(event.get()).expect("formatted event should be valid JSON")
}

#[test]
fn expired_sticky_format_keeps_the_key_without_a_remaining_time() {
	let sync: Raw<AnySyncTimelineEvent> =
		sticky_pdu(600_000).to_format_at(STICKY_START + 600_000);

	let event = formatted(sync.json());

	assert_eq!(event[STICKY_KEY], json!({ "duration_ms": 600_000 }));
	assert!(event["unsigned"].get(STICKY_TTL_KEY).is_none(), "expired copy kept a TTL");
}

#[test]
fn plain_format_has_neither_the_key_nor_a_remaining_time() {
	let sync: Raw<AnySyncTimelineEvent> = message_pdu().to_format_at(STICKY_START);
	let event = formatted(sync.json());

	assert!(event.get(STICKY_KEY).is_none(), "plain event gained the sticky key");
	assert!(event["unsigned"].get(STICKY_TTL_KEY).is_none(), "plain event gained a TTL");
}

#[test]
fn stored_copy_keeps_the_key_without_unsigned() {
	let stored = sticky_pdu(600_000).to_sync_message_like_without_unsigned();
	let event = formatted(stored.json());

	assert_eq!(event[STICKY_KEY], json!({ "duration_ms": 600_000 }));
	assert!(event.get("unsigned").is_none(), "stored copy carried unsigned");
}

#[test]
fn nested_sticky_copy_gains_its_own_remaining_time() {
	let pdu = message_pdu_with(&json!({
		"m.relations": {
			"m.thread": {
				"count": 1,
				"latest_event": {
					"event_id": "$latest:example.com",
					"origin_server_ts": STICKY_START,
					"msc4354_sticky": { "duration_ms": 600_000 },
				},
			},
		},
	}));

	let sync: Raw<AnySyncTimelineEvent> = pdu.to_format_at(STICKY_START + 100_000);
	let unsigned = formatted_unsigned(sync.json());
	let latest = &unsigned["m.relations"]["m.thread"]["latest_event"];

	assert_eq!(latest["unsigned"][STICKY_TTL_KEY], 500_000, "nested copy's remaining time");
	assert!(unsigned.get(STICKY_TTL_KEY).is_none(), "plain outer event gained a TTL");
}

#[test]
fn lapsed_copies_lose_a_remaining_time_formatted_earlier() {
	let pdu = message_pdu_with(&json!({
		STICKY_TTL_KEY: 1_000,
		"m.relations": {
			"m.thread": {
				"count": 1,
				"latest_event": {
					"event_id": "$latest:example.com",
					"origin_server_ts": STICKY_START,
					"msc4354_sticky": { "duration_ms": 600_000 },
					"unsigned": { STICKY_TTL_KEY: 1_000 },
				},
			},
		},
	}));

	let sync: Raw<AnySyncTimelineEvent> = pdu.to_format_at(STICKY_START + 700_000);
	let unsigned = formatted_unsigned(sync.json());
	let latest = &unsigned["m.relations"]["m.thread"]["latest_event"];

	assert!(
		latest["unsigned"].get(STICKY_TTL_KEY).is_none(),
		"lapsed nested copy kept a TTL"
	);
	assert!(unsigned.get(STICKY_TTL_KEY).is_none(), "plain outer event kept a TTL");
}

#[test]
fn built_unsigned_returns_to_the_event_and_its_json() {
	let pdu = Pdu { unsigned: None, ..message_pdu() };
	let pdu_json: CanonicalJsonObject =
		serde_json::from_value(json!({ "unsigned": { "age": 4 } })).expect("canonical object");

	let built = json!({ DELAY_ID: "delay" });
	let unsigned =
		Unsigned::from_json_string(built.to_string()).expect("valid raw unsigned object");

	let (pdu, pdu_json) = pdu
		.with_built_unsigned(pdu_json, Some(unsigned))
		.expect("an unsigned object");

	let restored: serde_json::Value = pdu
		.unsigned
		.as_ref()
		.map(|unsigned| serde_json::from_str(unsigned.json().get()))
		.transpose()
		.expect("readable unsigned data")
		.expect("unsigned data on the event");

	assert_eq!(restored, built);
	assert_eq!(
		serde_json::to_value(&pdu_json["unsigned"]).expect("serializable unsigned data"),
		json!({ "age": 4, DELAY_ID: "delay" })
	);
}
