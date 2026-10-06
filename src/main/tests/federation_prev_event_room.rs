#![cfg(test)]

use serde_json::json;
use tuwunel_core::{
	Err, Result,
	matrix::pdu::into_outgoing_federation,
	pdu::PduBuilder,
	ruma::{
		CanonicalJsonValue,
		events::{StateEventType, room::message::RoomMessageEventContent},
	},
};
use tuwunel_service::Services;

use self::{
	client::{Client, register},
	fixture::boot,
};

mod client;
mod fixture;

const TOKEN: &str = "federation-prev-event-room-access-token";

/// An incoming event cannot name another room's event as a prev event.
///
/// The other room's event is already in our timeline, so it is not fetched
/// again; it is still checked for its room, as a fetched prev event is.
/// Otherwise the state before the incoming event would be the other room's.
#[test]
fn prev_event_in_another_room_is_rejected() -> Result {
	let options: [&str; 0] = [];

	boot("federation-prev-event-room", options, exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let user_id = register(services, "prevevents", TOKEN).await?;
	let client = Client { services, base, token: TOKEN };
	let room_id = client.create_room(&json!({})).await?;
	let other_room_id = client.create_room(&json!({})).await?;
	let other_event_id = services
		.state_accessor
		.room_state_get_id(&other_room_id, &StateEventType::RoomMember, user_id.as_str())
		.await?;

	let room_version = services.state.get_room_version(&room_id).await?;
	let builder = PduBuilder::timeline(&RoomMessageEventContent::text_plain("hello"));
	let (_, mut pdu) = {
		let state_lock = services.state.mutex.lock(&room_id).await;

		services
			.timeline
			.create_hash_and_sign_event(builder, &user_id, &room_id, &state_lock)
			.await?
	};

	let prev_events = vec![CanonicalJsonValue::String(other_event_id.into())];

	pdu.insert("prev_events".into(), CanonicalJsonValue::Array(prev_events));

	let event_id = services
		.server_keys
		.gen_id_hash_and_sign_event(&mut pdu, &room_version)?;

	let pdu = into_outgoing_federation(pdu, &room_version);
	let result = services
		.event_handler
		.handle_incoming_pdu(services.globals.server_name(), &room_id, &event_id, pdu, true)
		.await;

	let Err(error) = result else {
		return Err!("an event with another room's prev event was accepted");
	};

	assert!(error.to_string().contains("wrong room"), "rejected for another reason: {error}");
	assert!(
		services
			.timeline
			.non_outlier_pdu_exists(&event_id)
			.await
			.is_err(),
		"the rejected event reached the timeline"
	);

	Ok(())
}
