#![cfg(test)]

use serde_json::{Value, json};
use tuwunel_core::{
	Result,
	ruma::{OwnedRoomId, events::StateEventType},
};
use tuwunel_service::Services;

use self::{
	client::{Client, field, register},
	fixture::boot,
};

mod client;
mod fixture;
#[path = "client/put.rs"]
mod put;
#[path = "client/state.rs"]
mod state;

const TOKEN: &str = "upgrade-rtc-slots-owner-access-token";

/// Room upgrades preserve every RTC slot under both event type names.
///
/// Open and closed slots retain their content and decoded slot IDs when a
/// version 11 room is upgraded to version 12.
#[test]
fn upgrade_rtc_slots() -> Result { boot("upgrade-rtc-slots", [], exercise) }

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "rtcowner", TOKEN).await?;

	let owner = Client { services, base, token: TOKEN };
	let room = owner
		.create_room(&json!({ "preset": "private_chat", "room_version": "11" }))
		.await?;

	let open = json!({ "status": "open", "application": { "type": "m.call" } });
	let closed = json!({ "status": "closed" });
	let slots = [
		("m.rtc.slot", "m.call#room", "m.call%23room", &open),
		("m.rtc.slot", "m.call#huddle", "m.call%23huddle", &closed),
		("org.matrix.msc4143.rtc.slot", "m.call#room", "m.call%23room", &open),
	];

	for (event_type, _, encoded_key, content) in slots {
		owner
			.put_state(&room, event_type, encoded_key, content)
			.await?;
	}

	let response = owner
		.post(&format!("rooms/{room}/upgrade"), &json!({ "new_version": "12" }))
		.await?;

	let replacement: OwnedRoomId = field(&response, "replacement_room")?.try_into()?;

	for (event_type, state_key, _, expected) in slots {
		let content = services
			.state_accessor
			.room_state_get_content::<Value>(
				&replacement,
				&StateEventType::from(event_type),
				state_key,
			)
			.await;

		assert_eq!(content.ok().as_ref(), Some(expected), "{event_type} at {state_key}");
	}

	Ok(())
}
