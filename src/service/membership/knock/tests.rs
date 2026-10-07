use ruma::{
	RoomId, RoomVersionId,
	api::federation::membership::{
		RawStrippedState, create_knock_event::v1::Response as SendKnockResponse,
	},
	events::StateEventType,
	room_id, user_id,
};
use serde_json::{Value, json, value::to_raw_value};
use tuwunel_core::{Result, config::Figment};

use crate::test_utils::fixture;

#[tokio::test]
#[tracing::instrument(level = "debug")]
async fn knock_state_does_not_set_local_memberships() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!knock:remote.invalid");
	let alice = user_id!("@alice:localhost");
	let response = SendKnockResponse::new(vec![
		pdu(room_id, "m.room.name", "", &json!({"name": "Knock"})),
		pdu(room_id, "m.room.member", alice.as_str(), &json!({"membership": "join"})),
	]);

	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.short
		.get_or_create_shortroomid(room_id)
		.await;

	let state = services
		.membership
		.ingest_send_knock_state(room_id, &response, &RoomVersionId::V11)
		.await?;

	services
		.membership
		.apply_send_knock_state(room_id, &state, &state_lock)
		.await?;

	services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomName, "")
		.await?;

	assert!(
		!services
			.state_cache
			.is_joined(alice, room_id)
			.await
	);

	Ok(())
}

fn pdu(room: &RoomId, kind: &str, state_key: &str, content: &Value) -> RawStrippedState {
	let event = json!({
		"room_id": room, "sender": "@bob:remote.invalid", "type": kind, "state_key": state_key,
		"content": content, "origin_server_ts": 1, "depth": 1, "prev_events": [],
		"auth_events": [], "hashes": {"sha256": "unchecked"},
	});

	RawStrippedState::Pdu(to_raw_value(&event).expect("valid json"))
}
