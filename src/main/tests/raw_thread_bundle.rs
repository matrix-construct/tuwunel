#![cfg(test)]

use futures::TryFutureExt;
use serde_json::{Value, json};
use tuwunel_core::{
	Result,
	ruma::{
		EventId, RoomId, UserId,
		api::federation::event::{
			get_event::v1::Request as EventRequest, get_room_state::v1::Request as StateRequest,
		},
		events::StateEventType,
	},
};
use tuwunel_service::Services;

use self::{
	client::{Client, register},
	federation::send,
	fixture::boot,
	room_state::state_id,
};

mod client;
#[expect(dead_code)] // Membership routes are not driven here.
mod federation;
mod fixture;
mod room_state;

#[path = "client/event.rs"]
mod event;
#[path = "client/get.rs"]
mod get;
#[expect(dead_code)] // Only the self-membership action is used from the add-on.
#[path = "client/membership.rs"]
mod membership;
#[path = "client/put.rs"]
mod put;
#[path = "client/room_event.rs"]
mod room_event;
#[path = "client/state.rs"]
mod state;
#[path = "client/topic.rs"]
mod topic;

const AUTHOR_TOKEN: &str = "raw-thread-bundle-author-access-token";
const READER_TOKEN: &str = "raw-thread-bundle-reader-access-token";

const ROOT: &str = "raw-thread-bundle root";
const REPLY: &str = "raw-thread-bundle reply";
const TOPIC: &str = "raw-thread-bundle topic";

/// No stored thread bundle leaves the server raw.
///
/// A thread root's stored bundle, and one planted on state events as rows from
/// earlier releases can hold, never reaches a peer: federation serves events
/// without unsigned data on every route, and in the fallback for an event
/// whose room version cannot be resolved. A member's own fetch still folds it.
#[test]
fn stored_thread_bundles_never_leave_raw() -> Result { boot("raw-thread-bundle", [], exercise) }

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, base: &str) -> Result {
	let author_id = register(services, "bundleauthor", AUTHOR_TOKEN).await?;

	register(services, "bundlereader", READER_TOKEN).await?;

	let author = Client { services, base, token: AUTHOR_TOKEN };
	let reader = Client { services, base, token: READER_TOKEN };
	let room = author
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	reader.act(&room, "join").await?;

	let topic = author.set_topic(&room, TOPIC).await?;
	let root = author.send_text(&room, "root", ROOT).await?;
	let content = json!({
		"msgtype": "m.text",
		"body": REPLY,
		"m.relates_to": { "rel_type": "m.thread", "event_id": root },
	});

	let reply = author
		.put_event(&format!("rooms/{room}/send/m.room.message/reply"), &content)
		.await?;

	assert!(bundled(services, &root).await?, "the reply left its root no stored bundle");

	let folded = reader.event(&room, &root).await?;
	let latest = &folded["unsigned"]["m.relations"]["m.thread"]["latest_event"];

	assert_eq!(latest["event_id"], reply.as_str(), "{folded}");

	let member =
		state_id(services, &room, &StateEventType::RoomMember, author_id.as_str()).await?;

	for event_id in [&topic, &member] {
		plant_bundle(services, event_id, &reply).await?;

		assert!(bundled(services, event_id).await?, "planting left {event_id} no bundle");
	}

	federation_sends_no_unsigned(services, base, &room, &root, &topic, &author_id).await
}

async fn bundled(services: &Services, event_id: &EventId) -> Result<bool> {
	services
		.timeline
		.get_pdu(event_id)
		.await?
		.has_thread_bundle()
}

/// Plants a thread bundle on a stored event, as earlier releases could leave
/// on any root.
///
/// The bundle's newest reply copies the reply's body and carries a
/// transaction ID.
async fn plant_bundle(services: &Services, event_id: &EventId, reply: &EventId) -> Result {
	let unsigned = json!({
		"m.relations": {
			"m.thread": {
				"count": 1,
				"current_user_participated": true,
				"latest_event": {
					"event_id": reply,
					"type": "m.room.message",
					"content": { "msgtype": "m.text", "body": REPLY },
					"unsigned": { "transaction_id": "reply" },
				},
			},
		},
	});

	let unsigned = serde_json::from_value(unsigned)?;
	let pdu_id = services.timeline.get_pdu_id(event_id).await?;
	let pdu_json = services
		.timeline
		.get_pdu_json(event_id)
		.map_ok(|mut pdu_json| {
			pdu_json.insert("unsigned".into(), unsigned);
			pdu_json
		})
		.await?;

	services
		.timeline
		.replace_pdu(&pdu_id, &pdu_json)
		.await
}

/// Federation serves the root and the planted state with no unsigned data.
///
/// That holds on `/event`, on `/state` with its auth chain, and in the
/// formatter's fallback for an event whose room version cannot be resolved.
async fn federation_sends_no_unsigned(
	services: &Services,
	base: &str,
	room: &RoomId,
	root: &EventId,
	topic: &EventId,
	author_id: &UserId,
) -> Result {
	let (status, event) = send(services, base, EventRequest::new(root.to_owned())).await?;

	assert_eq!(status, 200, "{event}");
	assert!(event.contains(ROOT), "{event}");
	assert_unsigned_omitted(&event);

	let request = StateRequest::new(root.to_owned(), room.to_owned());
	let (status, state) = send(services, base, request).await?;

	assert_eq!(status, 200, "{state}");
	assert!(state.contains(TOPIC), "{state}");
	assert_unsigned_omitted(&state);

	let state: Value = serde_json::from_str(&state)?;
	let planted = |list: &str| {
		entries(&state[list])
			.any(|pdu| pdu["type"] == "m.room.member" && pdu["state_key"] == author_id.as_str())
	};

	assert!(planted("pdus"), "the state omitted the planted member event: {state}");
	assert!(
		planted("auth_chain"),
		"the auth chain omitted the planted member event: {state}"
	);

	let unversioned = services
		.timeline
		.get_pdu_json(topic)
		.map_ok(|mut pdu_json| {
			pdu_json.remove("room_id");
			pdu_json
		})
		.await?;

	let fallback = services
		.federation
		.format_pdu_into(unversioned, None)
		.await;

	assert!(fallback.get().contains(TOPIC), "{fallback}");
	assert_unsigned_omitted(fallback.get());

	Ok(())
}

/// Asserts a federation body carries no unsigned data and no copy of the reply.
///
/// The whole body is scanned, so every event it serves counts.
fn assert_unsigned_omitted(body: &str) {
	assert!(!body.contains("\"unsigned\""), "{body}");
	assert!(!body.contains(REPLY), "{body}");
}

/// The elements of a JSON array.
///
/// Anything else yields none, so a missing list reads as an empty one.
fn entries(list: &Value) -> impl Iterator<Item = &Value> { list.as_array().into_iter().flatten() }
