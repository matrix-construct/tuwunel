#![cfg(test)]

use ruma::{
	EventId, RoomId, UserId,
	events::{AnySyncTimelineEvent, TimelineEventType},
	owned_room_id, owned_user_id,
	push::{Action, PushConditionRoomCtx, Ruleset},
	serde::Raw,
	uint,
};
use serde_json::Value as JsonValue;
use tuwunel_database::{Interfix, SEP, serialize_to_vec};

use super::{
	mindroom_push_suppressed, mindroom_terminal_push_content, mindroom_terminal_push_event,
};

const ROOM: &str = "!room:example.com";
const USER: &str = "@user:example.com";
const THREAD_ROOT_A: &str = "$thread_root_a";
const THREAD_ROOT_B: &str = "$thread_root_b";

fn room() -> &'static RoomId { ROOM.try_into().unwrap() }
fn user() -> &'static UserId { USER.try_into().unwrap() }
fn root_a() -> &'static EventId { THREAD_ROOT_A.try_into().unwrap() }
fn root_b() -> &'static EventId { THREAD_ROOT_B.try_into().unwrap() }

fn main_key() -> Vec<u8> { serialize_to_vec((user(), room())).expect("serialize main key") }
fn thread_key(root: &EventId) -> Vec<u8> {
	serialize_to_vec((user(), room(), root)).expect("serialize thread key")
}
fn interfix_prefix() -> Vec<u8> {
	serialize_to_vec((user(), room(), Interfix)).expect("serialize prefix")
}

fn stream_event(sender: &str, status: &str, msgtype: &str) -> Raw<AnySyncTimelineEvent> {
	serde_json::from_value(serde_json::json!({
		"content": {
			"body": "* final answer",
			"io.mindroom.stream_status": status,
			"m.new_content": {
				"body": "final answer",
				"io.mindroom.stream_status": status,
				"msgtype": msgtype,
			},
			"m.relates_to": {"event_id": "$original", "rel_type": "m.replace"},
			"msgtype": msgtype,
		},
		"event_id": "$edit",
		"origin_server_ts": 1,
		"sender": sender,
		"type": "m.room.message",
	}))
	.expect("valid timeline event")
}

fn encrypted_stream_event(sender: &str, status: &str, edit: bool) -> Raw<AnySyncTimelineEvent> {
	let mut content = serde_json::json!({
		"algorithm": "m.megolm.v1.aes-sha2",
		"ciphertext": "encrypted payload",
		"device_id": "DEVICE",
		"io.mindroom.stream_status": status,
		"sender_key": "sender key",
		"session_id": "session",
	});
	if edit {
		content
			.as_object_mut()
			.expect("content object")
			.insert(
				"m.relates_to".into(),
				serde_json::json!({
				"event_id": "$original",
				"rel_type": "m.replace",
				}),
			);
	}
	serde_json::from_value(serde_json::json!({
		"content": content,
		"event_id": "$encrypted_edit",
		"origin_server_ts": 1,
		"sender": sender,
		"type": "m.room.encrypted",
	}))
	.expect("valid encrypted timeline event")
}

/// Main `(user, room)` and thread `(user, room, root)` rows share a CF.
/// The `Interfix` prefix appends a trailing separator so a `starts_with`
/// scan matches only the longer 3-tuple shape.
#[test]
fn interfix_prefix_excludes_main_row() {
	let prefix = interfix_prefix();
	let main = main_key();

	assert!(!main.starts_with(&prefix), "Main 2-tuple row must not match thread prefix");
	assert_eq!(prefix.len(), main.len() + 1);
	assert_eq!(&prefix[..main.len()], &*main);
	assert_eq!(*prefix.last().unwrap(), SEP);
}

#[test]
fn interfix_prefix_includes_thread_row() {
	let prefix = interfix_prefix();
	let thread = thread_key(root_a());

	assert!(thread.starts_with(&prefix), "Thread 3-tuple row must match thread prefix");
}

#[test]
fn distinct_threads_have_distinct_keys() {
	assert_ne!(thread_key(root_a()), thread_key(root_b()));
}

/// Sweeping the 3-tuple prefix removes thread rows but not the main row,
/// per `clear_all_thread_notification_counts`.
#[test]
fn thread_prefix_sweep_preserves_main() {
	let prefix = interfix_prefix();
	let main = main_key();
	let a = thread_key(root_a());
	let b = thread_key(root_b());

	assert!(a.starts_with(&prefix));
	assert!(b.starts_with(&prefix));
	assert!(!main.starts_with(&prefix));
}

#[test]
fn terminal_mindroom_edit_is_normalized_for_push_rules() {
	let event = stream_event("@mindroom_helper:example.com", "completed", "m.text");
	let normalized = mindroom_terminal_push_event(&event).expect("terminal event");
	let value: JsonValue = serde_json::from_str(normalized.json().get()).expect("event json");
	let content = value["content"]
		.as_object()
		.expect("content object");

	assert!(!content.contains_key("m.relates_to"));
	assert!(!content.contains_key("m.new_content"));
	assert_eq!(content.get("body").and_then(JsonValue::as_str), Some("final answer"));
}

#[test]
fn nonterminal_stream_edit_is_not_normalized() {
	let streaming = stream_event("@mindroom_helper:example.com", "streaming", "m.notice");

	assert!(mindroom_terminal_push_event(&streaming).is_none());
}

#[test]
fn stream_protocol_is_sender_agnostic() {
	let remote = stream_event("@mindroom_helper:remote.example", "completed", "m.text");
	let ordinary_user = stream_event("@alice:example.com", "completed", "m.text");

	assert!(mindroom_terminal_push_event(&remote).is_some());
	assert!(mindroom_terminal_push_event(&ordinary_user).is_some());
}

#[test]
fn terminal_encrypted_edit_removes_only_the_exposed_relation() {
	let event = encrypted_stream_event("@mindroom_helper:example.com", "completed", true);
	let normalized = mindroom_terminal_push_event(&event).expect("terminal encrypted event");
	let value: JsonValue = serde_json::from_str(normalized.json().get()).expect("event json");
	let content = value["content"]
		.as_object()
		.expect("content object");

	assert!(!content.contains_key("m.relates_to"));
	assert_eq!(
		content
			.get("ciphertext")
			.and_then(JsonValue::as_str),
		Some("encrypted payload")
	);
	assert_eq!(
		content
			.get("io.mindroom.stream_status")
			.and_then(JsonValue::as_str),
		Some("completed")
	);
}

#[test]
fn encrypted_gateway_content_matches_push_rule_normalization() {
	let event = encrypted_stream_event("@mindroom_helper:example.com", "completed", true);
	let value: JsonValue = serde_json::from_str(event.json().get()).expect("event json");
	let content =
		mindroom_terminal_push_content(&TimelineEventType::RoomEncrypted, &value["content"])
			.expect("terminal encrypted content");

	assert!(content.get("m.relates_to").is_none());
	assert_eq!(
		content
			.get("ciphertext")
			.and_then(JsonValue::as_str),
		Some("encrypted payload")
	);
}

#[test]
fn nonterminal_stream_events_are_suppressed_for_any_sender() {
	let encrypted_pending =
		encrypted_stream_event("@mindroom_helper:example.com", "pending", false);
	let encrypted_streaming =
		encrypted_stream_event("@mindroom_helper:example.com", "streaming", true);
	let remote_pending =
		encrypted_stream_event("@mindroom_helper:remote.example", "pending", false);
	let future_intermediate =
		encrypted_stream_event("@mindroom_helper:example.com", "paused", true);

	assert!(mindroom_push_suppressed(&encrypted_pending));
	assert!(mindroom_push_suppressed(&encrypted_streaming));
	assert!(mindroom_push_suppressed(&remote_pending));
	assert!(mindroom_push_suppressed(&future_intermediate));
}

#[tokio::test]
async fn terminal_mindroom_edit_uses_normal_message_push_rule() {
	let user = owned_user_id!("@user:example.com");
	let ruleset = Ruleset::server_default(&user);
	let context = PushConditionRoomCtx::new(
		owned_room_id!("!room:example.com"),
		uint!(3),
		user,
		"User".into(),
	);
	let event = stream_event("@mindroom_helper:example.com", "completed", "m.text");

	assert!(
		ruleset
			.get_actions(&event, &context)
			.await
			.is_empty()
	);
	let normalized = mindroom_terminal_push_event(&event).expect("terminal event");
	assert!(
		ruleset
			.get_actions(&normalized, &context)
			.await
			.iter()
			.any(|action| matches!(action, Action::Notify))
	);
}

#[tokio::test]
async fn terminal_encrypted_edit_uses_encrypted_message_push_rule() {
	let user = owned_user_id!("@user:example.com");
	let ruleset = Ruleset::server_default(&user);
	let context = PushConditionRoomCtx::new(
		owned_room_id!("!room:example.com"),
		uint!(3),
		user,
		"User".into(),
	);
	let event = encrypted_stream_event("@mindroom_helper:example.com", "completed", true);

	assert!(
		ruleset
			.get_actions(&event, &context)
			.await
			.is_empty()
	);
	let normalized = mindroom_terminal_push_event(&event).expect("terminal encrypted event");
	assert!(
		ruleset
			.get_actions(&normalized, &context)
			.await
			.iter()
			.any(|action| matches!(action, Action::Notify))
	);
}
