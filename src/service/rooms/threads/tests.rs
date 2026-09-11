use ruma::{CanonicalJsonObject, events::AnySyncMessageLikeEvent, serde::Raw};
use serde_json::json;
use tuwunel_core::{
	matrix::pdu::{PduCount, PduId, RawPduId},
	utils::u64_from_u8,
};

use super::update_thread_bundle_raw;

mod append;

#[test]
fn backfilled_activity_key_sorts_before_normal() {
	let shortroomid = 0x0102_0304_0506_0708_u64;

	let normal: RawPduId = PduId { shortroomid, count: PduCount::Normal(1) }.into();

	let backfilled: RawPduId = PduId {
		shortroomid,
		count: PduCount::Backfilled(-5),
	}
	.into();

	assert_eq!(normal.shortroomid(), backfilled.shortroomid());
	assert!(backfilled.as_bytes() < normal.as_bytes());
}

#[test]
fn latest_count_value_round_trips() {
	let counts = [
		PduCount::Normal(1),
		PduCount::Normal(0x1112_1314_1516_1718),
		PduCount::Backfilled(0),
		PduCount::Backfilled(-42),
	];

	for count in counts {
		let read = PduCount::from_unsigned(u64_from_u8(&count.to_be_bytes()));

		assert_eq!(read, count);
	}
}

#[test]
fn thread_bundle_update_preserves_valid_siblings() {
	let mut unsigned: CanonicalJsonObject = serde_json::from_value(json!({
		"age": 4612,
		"m.relations": {
			"m.replace": { "event_id": "$edit:example.com" },
			"m.thread": {
				"count": 3,
				"current_user_participated": false,
				"latest_event": { "event_id": "$old:example.com" },
				"org.example.extra": true,
			},
		},
	}))
	.expect("test unsigned data should be canonical JSON");

	let latest_event: Raw<AnySyncMessageLikeEvent> = serde_json::from_value(json!({
		"type": "m.room.message",
		"content": { "msgtype": "m.text", "body": "latest" },
		"event_id": "$latest:example.com",
		"sender": "@alice:example.com",
		"origin_server_ts": 1,
	}))
	.expect("test latest event should be valid JSON");

	update_thread_bundle_raw(&mut unsigned, &latest_event);

	let unsigned =
		serde_json::to_value(unsigned).expect("updated unsigned data should be valid JSON");

	let thread = &unsigned["m.relations"]["m.thread"];

	assert_eq!(thread["count"], 4);
	assert_eq!(thread["current_user_participated"], false);
	assert_eq!(thread["latest_event"]["event_id"], "$latest:example.com");
	assert_eq!(thread["org.example.extra"], true);
	assert_eq!(unsigned["m.relations"]["m.replace"]["event_id"], "$edit:example.com");
	assert_eq!(unsigned["age"], 4612);
}

#[test]
fn thread_bundle_update_repairs_fields_without_clearing_siblings() {
	let mut unsigned: CanonicalJsonObject = serde_json::from_value(json!({
		"m.relations": {
			"m.thread": {
				"count": "invalid",
				"current_user_participated": null,
				"latest_event": null,
				"org.example.extra": { "kept": true },
			},
		},
	}))
	.expect("test unsigned data should be canonical JSON");

	let latest_event: Raw<AnySyncMessageLikeEvent> = serde_json::from_value(json!({
		"type": "m.room.message",
		"content": { "msgtype": "m.text", "body": "latest" },
		"event_id": "$latest:example.com",
		"sender": "@alice:example.com",
		"origin_server_ts": 1,
	}))
	.expect("test latest event should be valid JSON");

	update_thread_bundle_raw(&mut unsigned, &latest_event);

	let unsigned =
		serde_json::to_value(unsigned).expect("updated unsigned data should be valid JSON");

	let thread = &unsigned["m.relations"]["m.thread"];

	assert_eq!(thread["count"], 1);
	assert_eq!(thread["current_user_participated"], true);
	assert_eq!(thread["latest_event"]["event_id"], "$latest:example.com");
	assert_eq!(thread["org.example.extra"], json!({ "kept": true }));
}
