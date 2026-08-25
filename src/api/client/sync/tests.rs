use ruma::{events::AnyStrippedStateEvent, serde::Raw, user_id};
use serde_json::json;

use super::invite_sender;

#[test]
fn invite_sender_from_stripped_state() {
	let invite_state = stripped(&[
		json!({
			"type": "m.room.create",
			"state_key": "",
			"sender": "@founder:example.org",
			"content": {"room_version": "11", "creator": "@founder:example.org"},
		}),
		json!({
			"type": "m.room.member",
			"state_key": "@invitee:here.example",
			"sender": "@inviter:there.example",
			"content": {"membership": "invite"},
		}),
	]);

	let sender = invite_sender(user_id!("@invitee:here.example"), &invite_state);

	assert_eq!(sender.as_deref(), Some(user_id!("@inviter:there.example")));
	assert_eq!(invite_sender(user_id!("@other:here.example"), &invite_state), None);
}

#[test]
fn invite_sender_prefers_the_appended_genuine_event() {
	let invite_state = stripped(&[
		json!({
			"type": "m.room.member",
			"state_key": "@invitee:here.example",
			"sender": "@forged:there.example",
			"content": {"membership": "invite"},
		}),
		json!({
			"type": "m.room.member",
			"state_key": "@invitee:here.example",
			"sender": "@inviter:there.example",
			"content": {"membership": "invite"},
		}),
	]);

	let sender = invite_sender(user_id!("@invitee:here.example"), &invite_state);

	assert_eq!(sender.as_deref(), Some(user_id!("@inviter:there.example")));
}

#[test]
fn invite_sender_tolerates_undeserializable_events() {
	let invite_state = stripped(&[json!({"type": "m.room.member"})]);

	assert_eq!(invite_sender(user_id!("@invitee:here.example"), &invite_state), None);
}

fn stripped(events: &[serde_json::Value]) -> Vec<Raw<AnyStrippedStateEvent>> {
	events
		.iter()
		.map(|event| {
			Raw::new(event)
				.expect("valid json")
				.cast_unchecked()
		})
		.collect()
}
