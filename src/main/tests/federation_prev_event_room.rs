#![cfg(test)]

use serde_json::{json, value::to_raw_value};
use tuwunel_core::{
	Err, Result,
	matrix::{Event, pdu::into_outgoing_federation},
	pdu::PduBuilder,
	ruma::{
		CanonicalJsonObject, CanonicalJsonValue, EventId, MilliSecondsSinceUnixEpoch,
		OwnedEventId, RoomId, UInt, UserId,
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
/// names the other room's event. A backfilled event dated in the future must
/// not then make a later live event look old.
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

	let local_event_id = services
		.state_accessor
		.room_state_get_id(&room_id, &StateEventType::RoomMember, user_id.as_str())
		.await?;

	let (event_id, pdu) =
		sign_message(services, &user_id, &room_id, &[&other_event_id], None).await?;

	assert_rejected(services, &room_id, &event_id, pdu, "single").await?;

	let (event_id, pdu) =
		sign_message(services, &user_id, &room_id, &[&local_event_id, &other_event_id], None)
			.await?;

	assert_rejected(services, &room_id, &event_id, pdu, "multiple").await?;

	let first_ts = services
		.timeline
		.first_pdu_in_room(&room_id)
		.await?
		.origin_server_ts();

	let (prev_id, prev) =
		sign_message(services, &user_id, &room_id, &[&other_event_id], Some(first_ts)).await?;

	services
		.event_handler
		.handle_incoming_pdu(services.globals.server_name(), &room_id, &prev_id, prev, false)
		.await?;

	let (event_id, pdu) = sign_message(services, &user_id, &room_id, &[&prev_id], None).await?;

	assert_rejected(services, &room_id, &event_id, pdu, "boundary").await?;
	assert!(
		services
			.timeline
			.non_outlier_pdu_exists(&prev_id)
			.await
			.is_err(),
		"the stored prev event reached the timeline"
	);

	assert!(
		services
			.state
			.pdu_shortstatehash(&prev_id)
			.await
			.is_err(),
		"the rejected predecessor acquired a state association"
	);

	let (event_id, pdu) =
		sign_message(services, &user_id, &room_id, &[&local_event_id], None).await?;

	assert_accepted(services, &room_id, &event_id, pdu).await?;

	let (event_id, pdu) =
		sign_message(services, &user_id, &room_id, &[&local_event_id, &event_id], None).await?;

	assert_accepted(services, &room_id, &event_id, pdu).await?;

	let (prev_id, prev) =
		sign_message(services, &user_id, &room_id, &[&local_event_id], Some(first_ts)).await?;

	services
		.event_handler
		.handle_incoming_pdu(services.globals.server_name(), &room_id, &prev_id, prev, false)
		.await?;

	let (event_id, pdu) = sign_message(services, &user_id, &room_id, &[&prev_id], None).await?;

	assert_accepted(services, &room_id, &event_id, pdu).await?;
	assert!(
		services
			.timeline
			.non_outlier_pdu_exists(&prev_id)
			.await
			.is_ok(),
		"the same-room predecessor did not reach the timeline"
	);

	assert!(
		services
			.state
			.pdu_shortstatehash(&prev_id)
			.await
			.is_ok(),
		"the same-room predecessor has no state association"
	);

	assert_backfill_keeps_cutoff(services, &user_id, &room_id, &local_event_id).await
}

/// A backfilled event dated in the future does not make later live events
/// look old.
///
/// Backfilled events sort before the rest of the timeline, but the servers
/// that supplied them chose their timestamps.
async fn assert_backfill_keeps_cutoff(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	prev_id: &EventId,
) -> Result {
	let future_ts = MilliSecondsSinceUnixEpoch(UInt::new_saturating(8_000_000_000_000));
	let (backfill_id, backfill) =
		sign_message(services, user_id, room_id, &[prev_id], Some(future_ts)).await?;

	services
		.timeline
		.backfill_pdu(room_id, services.globals.server_name(), to_raw_value(&backfill)?)
		.await?;

	let first_id = services
		.timeline
		.first_pdu_in_room(room_id)
		.await?
		.event_id;

	assert_eq!(first_id, backfill_id, "the backfilled event is not the room's first");

	let (event_id, pdu) = sign_message(services, user_id, room_id, &[prev_id], None).await?;

	assert_accepted(services, room_id, &event_id, pdu).await
}

async fn sign_message(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	prev_event_ids: &[&EventId],
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

	let prev_events = prev_event_ids
		.iter()
		.map(|event_id| CanonicalJsonValue::String((*event_id).into()))
		.collect();

	pdu.insert("prev_events".into(), CanonicalJsonValue::Array(prev_events));

	let event_id = services
		.server_keys
		.gen_id_hash_and_sign_event(&mut pdu, &room_version)?;

	Ok((event_id, into_outgoing_federation(pdu, &room_version)))
}

async fn assert_rejected(
	services: &Services,
	room_id: &RoomId,
	event_id: &EventId,
	pdu: CanonicalJsonObject,
	case: &str,
) -> Result {
	let state = services
		.state
		.get_room_shortstatehash(room_id)
		.await?;

	let result = services
		.event_handler
		.handle_incoming_pdu(services.globals.server_name(), room_id, event_id, pdu, true)
		.await;

	let Err(error) = result else {
		return Err!("{case}: an event with another room's prev event was accepted");
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

	assert!(
		services
			.state
			.pdu_shortstatehash(event_id)
			.await
			.is_err(),
		"the rejected event acquired a state association"
	);

	let current_state = services
		.state
		.get_room_shortstatehash(room_id)
		.await?;

	assert_eq!(state, current_state);

	Ok(())
}

async fn assert_accepted(
	services: &Services,
	room_id: &RoomId,
	event_id: &EventId,
	pdu: CanonicalJsonObject,
) -> Result {
	let result = services
		.event_handler
		.handle_incoming_pdu(services.globals.server_name(), room_id, event_id, pdu, true)
		.await?;

	assert!(result.is_some(), "same-room event was not accepted");
	assert!(
		services
			.timeline
			.non_outlier_pdu_exists(event_id)
			.await
			.is_ok(),
		"the same-room event did not reach the timeline"
	);

	assert!(
		services
			.state
			.pdu_shortstatehash(event_id)
			.await
			.is_ok(),
		"the same-room event has no state association"
	);

	Ok(())
}
