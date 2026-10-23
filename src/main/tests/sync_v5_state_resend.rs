#![cfg(test)]

use serde_json::{Value, json, value::to_raw_value};
use tuwunel_core::{
	Result,
	ruma::{OwnedEventId, RoomId, UserId},
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
