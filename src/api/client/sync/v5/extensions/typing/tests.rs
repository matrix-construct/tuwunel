use ruma::{events::typing::SyncTypingEvent, room_id, serde::Raw, user_id};

use super::room_typing_event;

#[test]
fn room_typing_event_preserves_empty_user_list() {
	let room_id = room_id!("!room:example.com");
	let (_, raw) = room_typing_event(room_id, Vec::new()).expect("valid typing event");
	let event = raw
		.deserialize()
		.expect("typing event should deserialize");

	assert!(event.content.user_ids.is_empty());
}

#[test]
fn room_typing_event_preserves_typing_users() {
	let room_id = room_id!("!room:example.com");
	let user_id = user_id!("@alice:example.com").to_owned();
	let (_, raw): (_, Raw<SyncTypingEvent>) =
		room_typing_event(room_id, vec![user_id.clone()]).expect("valid typing event");
	let event = raw
		.deserialize()
		.expect("typing event should deserialize");

	assert_eq!(event.content.user_ids, [user_id]);
}
