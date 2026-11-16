#![cfg(test)]

use futures::TryFutureExt;
use reqwest::Url;
use serde_json::{Value, json};
use tokio::{
	net::TcpListener,
	spawn,
	sync::mpsc::{UnboundedReceiver, unbounded_channel},
};
use tuwunel_core::{
	Result, err, implement,
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
	client::{Client, accepted, register},
	federation::send,
	fixture::boot,
	room_state::state_id,
	stub::{Stub, parse, recv_txn, register as register_stub, serve},
	sync::joined_room,
};

mod client;
#[expect(dead_code)] // Membership routes are not driven here.
mod federation;
mod fixture;
mod room_state;

#[path = "appservice/stub.rs"]
mod stub;

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
#[path = "client/sync.rs"]
mod sync;
#[path = "client/topic.rs"]
mod topic;

const AUTHOR_TOKEN: &str = "raw-thread-bundle-author-access-token";
const READER_TOKEN: &str = "raw-thread-bundle-reader-access-token";

const ROOT: &str = "raw-thread-bundle root";
const REPLY: &str = "raw-thread-bundle reply";
const TOPIC: &str = "raw-thread-bundle topic";

const TIMELINE_ONE: &str = r#"{"room":{"timeline":{"limit":1}}}"#;
const LAZY_MEMBERS: &str = r#"{"lazy_load_members":true}"#;

const BRIDGE: &str = "rawbundle";

/// No stored thread bundle leaves the server raw.
///
/// A thread root's stored bundle, and one planted on state events as rows from
/// earlier releases can hold, never reaches a peer: federation serves events
/// without unsigned data on every route, and in the fallback for an event
/// whose room version cannot be resolved. The client paths that serve events
/// without folding bundles for the requester serve none, nor do appservice
/// transactions, while a member's own fetch still folds the bundle.
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

	author.send_text(&room, "later", "later").await?;

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

	federation_sends_no_unsigned(services, base, &room, &root, &topic, &author_id).await?;
	timeline_paths_serve_no_bundle(&reader, &room, &root).await?;
	state_paths_serve_no_bundle(&reader, &room, &root, &topic, &member).await?;
	appservice_receives_no_bundle(services, &root).await
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

/// The timeline paths that serve the root without folding carry no bundle.
///
/// That holds on `/notifications`, room `/initialSync` and the legacy event
/// stream.
#[tracing::instrument(level = "debug", skip_all)]
async fn timeline_paths_serve_no_bundle(
	reader: &Client<'_>,
	room: &RoomId,
	root: &EventId,
) -> Result {
	let notifications = reader.get("notifications").await?;
	let notified = entries(&notifications["notifications"]).map(|entry| &entry["event"]);

	assert_unbundled(served(notified, root)?);

	let initial = reader
		.get(&format!("rooms/{room}/initialSync"))
		.await?;

	assert_unbundled(served(entries(&initial["messages"]["chunk"]), root)?);

	let query = [("room_id", room.as_str()), ("from", "0"), ("timeout", "0")];
	let stream = reader.get_query("events", &query).await?;

	assert_unbundled(served(entries(&stream["chunk"]), root)?);

	Ok(())
}

/// The state sections serve the planted bundles nowhere.
///
/// That holds on sync, `/state`, a single state event, `/context`, `/members`,
/// room `/initialSync`, `/search` and the members `/messages` loads lazily.
#[tracing::instrument(level = "debug", skip_all)]
async fn state_paths_serve_no_bundle(
	reader: &Client<'_>,
	room: &RoomId,
	root: &EventId,
	topic: &EventId,
	member: &EventId,
) -> Result {
	let sync = reader
		.get_query("sync", &[("filter", TIMELINE_ONE)])
		.await?;

	let state = &joined_room(&sync, room)?["state"]["events"];

	assert_unbundled(served(entries(state), topic)?);
	assert_unbundled(served(entries(state), member)?);

	let room_state = reader.get(&format!("rooms/{room}/state")).await?;

	assert_unbundled(served(entries(&room_state), topic)?);

	let path = format!("rooms/{room}/state/m.room.topic/");
	let single = reader
		.get_query(&path, &[("format", "event")])
		.await?;

	assert_eq!(single["event_id"], topic.as_str(), "{single}");
	assert_unbundled(&single);

	let context = reader
		.get(&format!("rooms/{room}/context/{root}"))
		.await?;

	assert_unbundled(served(entries(&context["state"]), topic)?);

	let members = reader
		.get(&format!("rooms/{room}/members"))
		.await?;

	assert_unbundled(served(entries(&members["chunk"]), member)?);

	let initial = reader
		.get(&format!("rooms/{room}/initialSync"))
		.await?;

	assert_unbundled(served(entries(&initial["state"]), topic)?);

	let search = json!({
		"search_categories": {
			"room_events": { "search_term": "root", "include_state": true },
		},
	});

	let found = reader.post("search", &search).await?;
	let found_state = &found["search_categories"]["room_events"]["state"][room.as_str()];

	assert_unbundled(served(entries(found_state), topic)?);

	let query = [("dir", "b"), ("filter", LAZY_MEMBERS)];
	let messages = reader
		.get_query(&format!("rooms/{room}/messages"), &query)
		.await?;

	assert_unbundled(served(entries(&messages["state"]), member)?);

	Ok(())
}

/// An appservice transaction carries the root without its stored bundle.
///
/// The root is queued after the reply, as the appservice's interest would
/// queue it, so its stored row holds the bundle when the transaction is built.
#[tracing::instrument(level = "debug", skip_all)]
async fn appservice_receives_no_bundle(services: &Services, root: &EventId) -> Result {
	let listener = TcpListener::bind("127.0.0.1:0").await?;
	let url = format!("http://{}", listener.local_addr()?);
	let (sender, receiver) = unbounded_channel();
	let stub = spawn(serve(listener, sender));
	let bridge = Stub {
		id: BRIDGE,
		url: &url,
		sender_localpart: "_rawbundle",
		users: "@_rawbundle_.*",
		msc3202: false,
	};

	register_stub(services, &bridge).await?;

	let pdu_id = services.timeline.get_pdu_id(root).await?;

	services
		.sending
		.send_pdu_appservice(BRIDGE.to_owned(), pdu_id)?;

	let body = first_txn(receiver).await;

	stub.abort();

	assert_unbundled(served(entries(&parse(&body?)?["events"]), root)?);

	Ok(())
}

/// The body of the first transaction the stub receives.
///
/// The receiver is taken whole, since the test reads one transaction.
#[tracing::instrument(level = "debug", skip_all)]
async fn first_txn(mut receiver: UnboundedReceiver<(String, Vec<u8>)>) -> Result<Vec<u8>> {
	recv_txn(&mut receiver)
		.map_ok(|(_, body)| body)
		.await
}

/// Asserts a served event carries no stored aggregations and no copy of the
/// reply.
///
/// The whole event is scanned, so a copy outside `m.relations` counts too.
fn assert_unbundled(event: &Value) {
	assert!(event["unsigned"].get("m.relations").is_none(), "{event}");
	assert!(!event.to_string().contains(REPLY), "{event}");
}

/// The event among `events` with this ID.
///
/// A response that leaves the event out fails the test, naming the event.
fn served<'a, Events>(events: Events, event_id: &EventId) -> Result<&'a Value>
where
	Events: IntoIterator<Item = &'a Value>,
{
	events
		.into_iter()
		.find(|event| event["event_id"] == event_id.as_str())
		.ok_or_else(|| err!("{event_id} was not served"))
}

/// Get one endpoint path with query parameters as this user and parse the
/// reply.
///
/// The parameters are encoded for the URL, so a JSON filter passes as given; a
/// non-success status is the error.
#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn get_query(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
	let url = Url::parse_with_params(&self.url(path), query)?;

	accepted(self.get_url(url.as_str()).await?).await
}
