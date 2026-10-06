#![cfg(test)]

use serde_json::json;
use tuwunel_core::{
	Err, Result,
	matrix::{Event, pdu::into_outgoing_federation},
	pdu::PduBuilder,
	ruma::{
		CanonicalJsonObject, CanonicalJsonValue, EventId, MilliSecondsSinceUnixEpoch,
		OwnedEventId, RoomId, UserId,
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
/// The same holds when a stored prev event as old as the room's first event
/// names the other room's event.
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

	let (event_id, pdu) =
		sign_message(services, &user_id, &room_id, &other_event_id, None).await?;

	assert_rejected(services, &room_id, &event_id, pdu).await?;

	let first_ts = services
		.timeline
		.first_pdu_in_room(&room_id)
		.await?
		.origin_server_ts();

	let (prev_id, prev) =
		sign_message(services, &user_id, &room_id, &other_event_id, Some(first_ts)).await?;

	services
		.event_handler
		.handle_incoming_pdu(services.globals.server_name(), &room_id, &prev_id, prev, false)
		.await?;

	let (event_id, pdu) = sign_message(services, &user_id, &room_id, &prev_id, None).await?;

	assert_rejected(services, &room_id, &event_id, pdu).await?;
	assert!(
		services
			.timeline
			.non_outlier_pdu_exists(&prev_id)
			.await
			.is_err(),
		"the stored prev event reached the timeline"
	);

	Ok(())
}

/// Sign a message for `room_id` whose only prev event is `prev_event_id`.
async fn sign_message(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	prev_event_id: &EventId,
	timestamp: Option<MilliSecondsSinceUnixEpoch>,
) -> Result<(OwnedEventId, CanonicalJsonObject)> {
	let room_version = services.state.get_room_version(room_id).await?;
	let builder = PduBuilder {
		timestamp,
		..PduBuilder::timeline(&RoomMessageEventContent::text_plain("hello"))
	};

	let (_, mut pdu) = {
		let state_lock = services.state.mutex.lock(room_id).await;

		services
			.timeline
			.create_hash_and_sign_event(builder, user_id, room_id, &state_lock)
			.await?
	};

	let prev_events = vec![CanonicalJsonValue::String(prev_event_id.into())];

	pdu.insert("prev_events".into(), CanonicalJsonValue::Array(prev_events));

	let event_id = services
		.server_keys
		.gen_id_hash_and_sign_event(&mut pdu, &room_version)?;

	Ok((event_id, into_outgoing_federation(pdu, &room_version)))
}

/// Hand `pdu` over as an incoming timeline event, which must be rejected for
/// its prev event's room and kept out of the timeline.
async fn assert_rejected(
	services: &Services,
	room_id: &RoomId,
	event_id: &EventId,
	pdu: CanonicalJsonObject,
) -> Result {
	let result = services
		.event_handler
		.handle_incoming_pdu(services.globals.server_name(), room_id, event_id, pdu, true)
		.await;

	let Err(error) = result else {
		return Err!("an event with another room's prev event was accepted");
	};

	assert!(error.to_string().contains("wrong room"), "rejected for another reason: {error}");
	assert!(
		services
			.timeline
			.non_outlier_pdu_exists(event_id)
			.await
			.is_err(),
		"the rejected event reached the timeline"
	);

	Ok(())
}
