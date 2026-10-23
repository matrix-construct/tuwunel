#![cfg(test)]

use serde_json::{Map, Value, json, value::to_raw_value};
use tuwunel_core::{
	Result, err,
	ruma::{OwnedEventId, OwnedRoomId, RoomId, UserId},
	utils::BoolExt,
};
use tuwunel_matrix::pdu::PduBuilder;
use tuwunel_service::Services;

use self::{
	client::{Client, register},
	fixture::boot,
	required_state::expect_state,
};

mod client;
mod fixture;
mod sliding_sync;

#[path = "client/message.rs"]
mod message;

#[path = "client/put.rs"]
mod put;

#[path = "sliding_sync/required_state.rs"]
mod required_state;

#[path = "sliding_sync/resume.rs"]
mod resume;

#[path = "client/state.rs"]
mod state;

const TOKEN: &str = "sync-v5-state-resend-test-access-token";
const BYSTANDER_TOKEN: &str = "sync-v5-state-resend-bystander-access-token";
const CALL_MEMBER: &str = "org.matrix.msc3401.call.member";
const TOPIC: &str = "m.room.topic";
const COUNTED_KEY: &str = "counted";
const OUTLIER_KEY: &str = "outlier";

/// Delivers state without a timeline position once, then only its changes.
///
/// State a remote join installs is stored as outliers, so it has no timeline
/// position to compare with the room's cursor. It is judged against the snapshot
/// of state the client was last sent instead: an unrelated change leaves it out,
/// and an outlier made current later still reaches the client. A connection
/// stored before snapshots were recorded resends such state once rather than
/// miss it.
#[test]
fn outlier_state_is_resent_only_when_it_changes() -> Result {
	boot("sync-v5-outlier-state", ["log_enable=false"], outliers)
}

async fn outliers(services: &Services, base: &str) -> Result {
	let user = register(services, "outliers", TOKEN).await?;
	let client = Client { services, base, token: TOKEN };
	let room = client
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	let counted = client
		.put_state(&room, CALL_MEMBER, COUNTED_KEY, &json!({ "revision": 1 }))
		.await?;

	let outlier = install_outlier(services, &user, &room, 1).await?;
	let request = sync_request();
	let initial = client.sync(None, &request).await?;

	expect_state(&initial, &room, &[counted.as_str(), outlier.as_str()], "initial")?;

	client
		.put_state(&room, TOPIC, "", &json!({ "topic": "unrelated" }))
		.await?;

	let unrelated = client.sync(Some(&initial), &request).await?;

	expect_state(&unrelated, &room, &[], "unrelated state change")?;

	let recounted = client
		.put_state(&room, CALL_MEMBER, COUNTED_KEY, &json!({ "revision": 2 }))
		.await?;

	let changed = client.sync(Some(&unrelated), &request).await?;

	expect_state(&changed, &room, &[recounted.as_str()], "changed counted state")?;

	let replaced = install_outlier(services, &user, &room, 2).await?;

	client
		.put_state(&room, TOPIC, "", &json!({ "topic": "after the outlier" }))
		.await?;

	let resolved = client.sync(Some(&changed), &request).await?;

	expect_state(&resolved, &room, &[replaced.as_str()], "outlier made current")?;

	forget_delivered_snapshots(services).await?;

	let unrecorded = install_outlier(services, &user, &room, 3).await?;

	client
		.send_text(&room, "after-the-outlier", "after the outlier")
		.await?;

	let messaged = client.sync(Some(&resolved), &request).await?;

	expect_state(&messaged, &room, &[], "message without a snapshot")?;

	client
		.put_state(&room, TOPIC, "", &json!({ "topic": "before snapshots" }))
		.await?;

	let legacy = client.sync(Some(&messaged), &request).await?;

	expect_state(&legacy, &room, &[unrecorded.as_str()], "outlier without a snapshot")?;

	client
		.put_state(&room, TOPIC, "", &json!({ "topic": "recorded again" }))
		.await?;

	let recorded = client.sync(Some(&legacy), &request).await?;

	expect_state(&recorded, &room, &[], "snapshot recorded again")
}

/// Sign a state event and install it as current state without a timeline
/// position.
///
/// This is how a remote join leaves the state it received, and how state
/// resolution can select state fetched to fill a gap: stored as an outlier and
/// made current by a new state snapshot.
async fn install_outlier(
	services: &Services,
	user: &UserId,
	room: &RoomId,
	revision: u64,
) -> Result<OwnedEventId> {
	let event = PduBuilder {
		event_type: CALL_MEMBER.into(),
		content: to_raw_value(&json!({ "revision": revision }))?.into(),
		state_key: Some(OUTLIER_KEY.into()),
		..Default::default()
	};

	let state_lock = services.state.mutex.lock(room).await;
	let (pdu, pdu_json) = services
		.timeline
		.create_hash_and_sign_event(event, user, room, &state_lock)
		.await?;

	services
		.timeline
		.add_pdu_outlier(&pdu.event_id, &pdu_json);

	let shortstatehash = services.state.append_to_state(&pdu).await?;

	services
		.state
		.set_room_state(room, shortstatehash, &state_lock);

	Ok(pdu.event_id)
}

fn sync_request() -> Value {
	json!({
		"conn_id": "state-resend",
		"lists": { "all": {
			"ranges": [[0, 9]], "required_state": [[CALL_MEMBER, "*"]], "timeline_limit": 1,
		} },
	})
}

/// Clear the snapshot each loaded connection's rooms were last delivered against.
///
/// A zero snapshot is what a connection stored before snapshots were recorded holds.
async fn forget_delivered_snapshots(services: &Services) -> Result {
	for key in services.sync.list_loaded_connections().await {
		services
			.sync
			.get_loaded_connection(&key)
			.await?
			.lock()
			.await
			.rooms
			.values_mut()
			.for_each(|room| room.shortstatehash = 0);
	}

	Ok(())
}

/// Answers a retried position with what the lost response carried.
///
/// matrix-rust-sdk resends its last position whenever it abandons a request,
/// and a request that ran a pass before it was abandoned has already advanced
/// every room in its window. A retry returns those rooms to where the client
/// left them, so a quiet room stays out of the reply and a busy room repeats
/// only its new events.
#[test]
fn retried_position_repeats_only_the_lost_delta() -> Result {
	boot("sync-v5-retried-position", ["log_enable=false"], retries)
}

async fn retries(services: &Services, base: &str) -> Result {
	register(services, "retries", TOKEN).await?;
	register(services, "bystander", BYSTANDER_TOKEN).await?;

	let client = Client { services, base, token: TOKEN };
	let bystander = Client { services, base, token: BYSTANDER_TOKEN };
	let busy = room_with_call_member(&client).await?;
	let quiet = room_with_call_member(&client).await?;
	let request = sync_request();
	let initial = client.sync(None, &request).await?;
	let message_id = client.send_text(&busy, "news", "news").await?;
	let answered = client.sync(Some(&initial), &request).await?;
	let retried = client.sync(Some(&initial), &request).await?;

	for (response, context) in [(&answered, "answered"), (&retried, "retried")] {
		expect_state(response, &busy, &[], context)?;
		expect_timeline(response, &busy, message_id.as_str(), context)?;
		expect_absent(response, &quiet, context)?;
	}

	assert_eq!(
		rooms_without_ages(&retried),
		rooms_without_ages(&answered),
		"retry changed room payloads"
	);

	bystander.create_room(&json!({})).await?;

	let idle = client.sync(Some(&retried), &request).await?;
	let idle_retried = client.sync(Some(&retried), &request).await?;

	for (response, context) in [(&idle, "idle"), (&idle_retried, "idle retried")] {
		expect_absent(response, &busy, context)?;
		expect_absent(response, &quiet, context)?;
	}

	Ok(())
}

async fn room_with_call_member(client: &Client<'_>) -> Result<OwnedRoomId> {
	let room = client
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	client
		.put_state(&room, CALL_MEMBER, COUNTED_KEY, &json!({ "revision": 1 }))
		.await?;

	Ok(room)
}

fn expect_timeline(response: &Value, room: &RoomId, event_id: &str, context: &str) -> Result {
	let carried = response["rooms"][room.as_str()]["timeline"]
		.as_array()
		.into_iter()
		.flatten()
		.any(|event| event["event_id"] == event_id);

	BoolExt::ok_or_else(carried, || err!("{context}: timeline lacks {event_id}"))
}

fn expect_absent(response: &Value, room: &RoomId, context: &str) -> Result {
	let payload = &response["rooms"][room.as_str()];

	BoolExt::ok_or_else(payload.is_null(), || err!("{context}: unexpected {payload}"))
}

// Event ages vary with the moment each reply was built.
fn rooms_without_ages(response: &Value) -> Value {
	let mut rooms = response["rooms"].clone();

	rooms
		.as_object_mut()
		.into_iter()
		.flat_map(Map::values_mut)
		.filter_map(|room| {
			room.get_mut("timeline")
				.and_then(Value::as_array_mut)
		})
		.flatten()
		.filter_map(|event| {
			event
				.get_mut("unsigned")
				.and_then(Value::as_object_mut)
		})
		.for_each(|unsigned| {
			unsigned.remove("age");
		});

	rooms
}
