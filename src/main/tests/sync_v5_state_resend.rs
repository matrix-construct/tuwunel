#![cfg(test)]

use serde_json::{Map, Value, json, value::to_raw_value};
use tuwunel_core::{
	Result, err, implement,
	ruma::{CanonicalJsonObject, OwnedEventId, OwnedRoomId, RoomId, UserId},
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

#[path = "client/topic.rs"]
mod topic;

const TOKEN: &str = "sync-v5-state-resend-test-access-token";
const BYSTANDER_TOKEN: &str = "sync-v5-state-resend-bystander-access-token";
const CALL_MEMBER: &str = "org.matrix.msc3401.call.member";
const NAME: &str = "m.room.name";
const COUNTED_KEY: &str = "counted";
const OUTLIER_KEY: &str = "outlier";
const EVENT_IDS: &str = "shorteventid_eventid";
const STATE_KEYS: &str = "shortstatekey_statekey";
const PDUS: &str = "pduid_pdu";
const UNREADABLE: &[u8] = b"unreadable";

/// One room's sliding sync replies, each checked for its required state.
///
/// It carries the test client and the room, so each step of a test is one call.
struct Replies<'a> {
	client: &'a Client<'a>,
	room: &'a RoomId,
}

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
	let request = sync_request("*");
	let replies = Replies { client: &client, room: &room };
	let initial = replies
		.expect_next(None, &request, &[counted.as_str(), outlier.as_str()], "initial")
		.await?;

	client.set_topic(&room, "unrelated").await?;

	let unrelated = replies
		.expect_next(Some(&initial), &request, &[], "unrelated state change")
		.await?;

	let recounted = client
		.put_state(&room, CALL_MEMBER, COUNTED_KEY, &json!({ "revision": 2 }))
		.await?;

	let changed = replies
		.expect_next(Some(&unrelated), &request, &[recounted.as_str()], "changed counted state")
		.await?;

	let replaced = install_outlier(services, &user, &room, 2).await?;

	client
		.set_topic(&room, "after the outlier")
		.await?;

	let resolved = replies
		.expect_next(Some(&changed), &request, &[replaced.as_str()], "outlier made current")
		.await?;

	forget_delivered_snapshots(services).await?;

	let unrecorded = install_outlier(services, &user, &room, 3).await?;

	client
		.send_text(&room, "after-the-outlier", "after the outlier")
		.await?;

	let messaged = replies
		.expect_next(Some(&resolved), &request, &[], "message without a snapshot")
		.await?;

	client
		.set_topic(&room, "before snapshots")
		.await?;

	let legacy = replies
		.expect_next(
			Some(&messaged),
			&request,
			&[unrecorded.as_str()],
			"outlier without a snapshot",
		)
		.await?;

	replies
		.expect_recorded(&legacy, &request)
		.await
		.map(drop)
}

/// Sync once and fail unless the room came back with exactly the expected
/// required state.
///
/// The reply is returned, so the next step resumes from its position.
#[implement(Replies, params = "<'_>")]
async fn expect_next(
	&self,
	since: Option<&Value>,
	request: &Value,
	expected: &[&str],
	context: &str,
) -> Result<Value> {
	let reply = self.client.sync(since, request).await?;

	expect_state(&reply, self.room, expected, context).map(|()| reply)
}

/// Change the topic and fail unless the room's next reply carries no state.
///
/// A delivery that read every requested event vouches for all of them, so an
/// unrelated state change after it resends none.
#[implement(Replies, params = "<'_>")]
async fn expect_recorded(&self, since: &Value, request: &Value) -> Result<Value> {
	self.client
		.set_topic(self.room, "recorded again")
		.await?;

	self.expect_next(Some(since), request, &[], "snapshot recorded again")
		.await
}

/// Sends state whose event could not be read, on the first state change after
/// the event is stored.
///
/// Current state can name an event whose PDU cannot be read, and the reply
/// leaves that event out. The event arrives once stored whether it was first
/// due in a full reply or newly requested by a widened selector.
#[test]
fn unreadable_state_is_sent_once_readable() -> Result {
	boot("sync-v5-unreadable-state", ["log_enable=false"], unreadable)
}

async fn unreadable(services: &Services, base: &str) -> Result {
	let user = register(services, "unreadable", TOKEN).await?;
	let client = Client { services, base, token: TOKEN };
	let room = client
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	let (missing, pdu) = install_unreadable_state(services, &user, &room, 1).await?;
	let wide = sync_request("*");
	let replies = Replies { client: &client, room: &room };
	let initial = replies
		.expect_next(None, &wide, &[], "unreadable outlier")
		.await?;

	services.timeline.add_pdu_outlier(&missing, &pdu);
	client.set_topic(&room, "unrelated").await?;

	let stored = replies
		.expect_next(Some(&initial), &wide, &[missing.as_str()], "outlier stored")
		.await?;

	let recorded = replies.expect_recorded(&stored, &wide).await?;

	let narrow = sync_request(COUNTED_KEY);
	let narrowed = replies
		.expect_next(Some(&recorded), &narrow, &[], "narrowed selector")
		.await?;

	let (missing, pdu) = install_unreadable_state(services, &user, &room, 2).await?;

	client
		.set_topic(&room, "beside an unread outlier")
		.await?;

	let unselected = replies
		.expect_next(Some(&narrowed), &narrow, &[], "unread outlier not selected")
		.await?;

	let widened = replies
		.expect_next(Some(&unselected), &wide, &[], "widened onto an unread outlier")
		.await?;

	services.timeline.add_pdu_outlier(&missing, &pdu);
	client.set_topic(&room, "after widening").await?;

	replies
		.expect_next(Some(&widened), &wide, &[missing.as_str()], "widened outlier stored")
		.await
		.map(drop)
}

/// Sends counted state whose event could not be read, once it reads again.
///
/// State with a timeline position is compared with the snapshot the client was
/// last sent, and that snapshot holds the event the reply left out because it
/// could not be read. The event arrives on the first state change after it
/// reads again.
#[test]
fn unreadable_counted_state_is_sent_once_readable() -> Result {
	boot("sync-v5-unreadable-counted-state", ["log_enable=false"], unreadable_counted)
}

async fn unreadable_counted(services: &Services, base: &str) -> Result {
	register(services, "counted", TOKEN).await?;

	let client = Client { services, base, token: TOKEN };
	let room = client
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	let counted = client
		.put_state(&room, CALL_MEMBER, COUNTED_KEY, &json!({ "revision": 1 }))
		.await?;

	// Two later events keep it out of the one-event timeline and the event read to see
	// whether that timeline is limited.
	client.send_text(&room, "first", "first").await?;
	client
		.send_text(&room, "second", "second")
		.await?;

	let pdu_id = services.timeline.get_pdu_id(&counted).await?;
	let stored = replace_row(services, PDUS, pdu_id.as_bytes(), Some(UNREADABLE)).await?;

	let request = sync_request("*");
	let replies = Replies { client: &client, room: &room };
	let initial = replies
		.expect_next(None, &request, &[], "unreadable counted event")
		.await?;

	services.db[PDUS].insert(&pdu_id, stored);
	client.set_topic(&room, "unrelated").await?;

	let read = replies
		.expect_next(Some(&initial), &request, &[counted.as_str()], "counted event readable")
		.await?;

	replies
		.expect_recorded(&read, &request)
		.await
		.map(drop)
}

/// Overwrite one database row, or remove it, returning the value it held.
///
/// Writing the returned value back under the same key restores the row.
async fn replace_row(
	services: &Services,
	name: &str,
	key: &[u8],
	value: Option<&[u8]>,
) -> Result<Vec<u8>> {
	let map = &services.db[name];
	let stored = map.get(key).await?.to_vec();

	match value {
		| Some(value) => map.insert(key, value),
		| None => map.remove(key),
	}

	Ok(stored)
}

/// Sends state whose short ID mapping could not be read, once it reads again.
///
/// A wildcard selector lists a type's state through its short ID mappings, and
/// an entry whose mapping cannot be read is left out of the reply. The event
/// arrives on the first state change after the mapping is restored, whether its
/// event ID or its state key failed to resolve.
#[test]
fn unmapped_state_is_sent_once_mapped() -> Result {
	boot("sync-v5-unmapped-state", ["log_enable=false"], unmapped)
}

async fn unmapped(services: &Services, base: &str) -> Result {
	let user = register(services, "unmapped", TOKEN).await?;
	let client = Client { services, base, token: TOKEN };
	let room = client
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	let outlier = install_outlier(services, &user, &room, 1).await?;
	let short_event_id = services
		.short
		.get_shorteventid(&outlier)
		.await?
		.to_be_bytes();

	let event_id = replace_row(services, EVENT_IDS, &short_event_id, None).await?;
	let request = sync_request("*");
	let replies = Replies { client: &client, room: &room };
	let initial = replies
		.expect_next(None, &request, &[], "unmapped event ID")
		.await?;

	services.db[EVENT_IDS].insert(&short_event_id, event_id);
	client.set_topic(&room, "event ID mapped").await?;

	let mapped = replies
		.expect_next(Some(&initial), &request, &[outlier.as_str()], "event ID mapped")
		.await?;

	let replaced = install_outlier(services, &user, &room, 2).await?;
	let short_state_key = services
		.short
		.get_shortstatekey(&CALL_MEMBER.into(), OUTLIER_KEY)
		.await?
		.to_be_bytes();

	let type_and_key = replace_row(services, STATE_KEYS, &short_state_key, None).await?;

	client
		.set_topic(&room, "beside an unmapped state key")
		.await?;

	let hidden = replies
		.expect_next(Some(&mapped), &request, &[], "unmapped state key")
		.await?;

	services.db[STATE_KEYS].insert(&short_state_key, type_and_key);
	client
		.set_topic(&room, "state key mapped")
		.await?;

	let remapped = replies
		.expect_next(Some(&hidden), &request, &[replaced.as_str()], "state key mapped")
		.await?;

	replies
		.expect_recorded(&remapped, &request)
		.await
		.map(drop)
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
	let (event_id, pdu) = install_unreadable_state(services, user, room, revision).await?;

	services.timeline.add_pdu_outlier(&event_id, &pdu);

	Ok(event_id)
}

/// Sign a state event and install it as current state without storing its PDU.
///
/// The state names an event whose PDU cannot be read; storing the returned PDU
/// as an outlier makes it readable under the same ID.
async fn install_unreadable_state(
	services: &Services,
	user: &UserId,
	room: &RoomId,
	revision: u64,
) -> Result<(OwnedEventId, CanonicalJsonObject)> {
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

	let shortstatehash = services.state.append_to_state(&pdu).await?;

	services
		.state
		.set_room_state(room, shortstatehash, &state_lock);

	Ok((pdu.event_id, pdu_json))
}

/// A request for the call member state under one state key, or all of it.
///
/// It also asks for a room name these rooms never set, so every reply looks up
/// a key that current state does not hold; a missing key is not an unread event.
fn sync_request(state_key: &str) -> Value {
	json!({
		"conn_id": "state-resend",
		"lists": { "all": {
			"ranges": [[0, 9]],
			"required_state": [[CALL_MEMBER, state_key], [NAME, ""]],
			"timeline_limit": 1,
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
	let request = sync_request("*");
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
