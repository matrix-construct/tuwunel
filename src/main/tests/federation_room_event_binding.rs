#![cfg(test)]

use std::collections::BTreeMap;

use futures::TryStreamExt;
use serde_json::{Value, json};
use tuwunel_core::{
	Result, at, err, implement,
	ruma::{
		CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId, RoomId, RoomVersionId,
		UserId,
		api::federation::{
			authorization::get_event_authorization::v1::Request as EventAuthRequest,
			event::{
				get_room_state::v1::Request as StateRequest,
				get_room_state_ids::v1::Request as StateIdsRequest,
			},
			membership::create_join_event::v2::Request as JoinRequest,
		},
		events::{StateEventType, room::member::MembershipState},
		exports::serde::Deserialize,
	},
	utils::stream::{IterStream, TryBroadbandExt},
};
use tuwunel_matrix::{Event, event::gen_event_id};
use tuwunel_service::Services;

use self::{
	client::{Client, register},
	federation::{prepare, send, submit},
	fixture::boot,
};

mod client;
mod federation;
mod fixture;

#[path = "client/event.rs"]
mod event;
#[path = "client/put.rs"]
mod put;

const ROUTES: [Route; 3] = [Route::State, Route::StateIds, Route::EventAuth];

const OWNER_TOKEN: &str = "room-event-binding-owner-access-token";
const MEMBER_TOKEN: &str = "room-event-binding-member-access-token";
const JOINER_TOKEN: &str = "room-event-binding-joiner-access-token";

/// Bytes stored over an event's rows, which a read finds but cannot decode.
///
/// They are not JSON, so the read fails rather than finding nothing.
const UNREADABLE: &[u8] = b"unreadable";

/// A room version this server does not support, so every auth chain walk fails.
///
/// No client request can create such a room, so the test writes the version
/// into a stored create event instead.
const UNSUPPORTED_VERSION: &str = "org.example.unsupported";

type Served = BTreeMap<OwnedEventId, AuthEvents>;

type ServedEvent = (OwnedEventId, AuthEvents);

type AuthEvents = Vec<OwnedEventId>;

type Fields<'a, T> = BTreeMap<&'a str, Vec<T>>;

#[derive(Clone, Copy, Debug)]
enum Route {
	State,
	StateIds,
	EventAuth,
}

/// The two event lists a `send_join` response serves.
///
/// The response also carries the join event itself, which the omission checks
/// do not read.
#[derive(Deserialize)]
#[serde(crate = "tuwunel_core::ruma::exports::serde")]
struct JoinState {
	state: Vec<CanonicalJsonObject>,
	auth_chain: Vec<CanonicalJsonObject>,
}

/// The federation `/state`, `/state_ids` and `/event_auth` routes answer for
/// the requested room alone.
///
/// An event from another room is as unknown as one that does not exist. The
/// auth chain beside a room's state carries every auth event that state
/// names, so a peer holding nothing else can authorise each state event.
///
/// An event that storage cannot read drops out of a `/state` or `send_join`
/// response rather than failing it. An auth chain that cannot be walked drops
/// out of each route's response the same way.
#[test]
fn federation_routes_bind_events_to_rooms() -> Result { boot("room-event-binding", [], exercise) }

async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "binding", OWNER_TOKEN).await?;

	let member_id = register(services, "bindingmember", MEMBER_TOKEN).await?;
	let owner = Client { services, base, token: OWNER_TOKEN };
	let member = Client { services, base, token: MEMBER_TOKEN };

	let room = owner.create_room(&json!({})).await?;
	let event = state_id(services, &room, &StateEventType::RoomGuestAccess, "").await?;
	let room_v12 = owner
		.create_room(&json!({ "room_version": "12" }))
		.await?;

	let event_v12 = state_id(services, &room_v12, &StateEventType::RoomGuestAccess, "").await?;
	let create_v12 = state_id(services, &room_v12, &StateEventType::RoomCreate, "").await?;
	let unknown_id = EventId::parse("$room-event-binding-unknown")?;

	for route in ROUTES {
		route
			.get_ok(services, base, &room, &event)
			.await?;

		let foreign = route
			.get(services, base, &room, &event_v12)
			.await?;

		let unknown = route
			.get(services, base, &room, &unknown_id)
			.await?;

		assert_eq!(foreign.0, 404);
		assert_eq!(unknown, foreign);

		route
			.get_ok(services, base, &room_v12, &event_v12)
			.await?;
	}

	Route::EventAuth
		.get_ok(services, base, &room_v12, &create_v12)
		.await?;

	for version in ["11", "12"] {
		auth_chain_closes_over_state(&owner, &member, &member_id, version).await?;
	}

	let joiner_id = register(services, "bindingjoiner", JOINER_TOKEN).await?;

	unreadable_events_are_omitted(&owner, &member, &member_id, &joiner_id).await?;
	failed_walk_omits_the_auth_chain(&owner).await
}

/// Request this route at an event and return the body of its 200 answer.
///
/// Any other status fails the test, naming the route and the body the server
/// answered with.
#[implement(Route)]
async fn get_ok(
	self,
	services: &Services,
	base: &str,
	room_id: &RoomId,
	event_id: &EventId,
) -> Result<String> {
	let (status, body) = self
		.get(services, base, room_id, event_id)
		.await?;

	assert_eq!(status, 200, "{self:?}: {body}");

	Ok(body)
}

#[implement(Route)]
async fn get(
	self,
	services: &Services,
	base: &str,
	room_id: &RoomId,
	event_id: &EventId,
) -> Result<(u16, String)> {
	match self {
		| Self::State => {
			let request = StateRequest::new(event_id.to_owned(), room_id.to_owned());

			send(services, base, request).await
		},
		| Self::StateIds => {
			let request = StateIdsRequest::new(event_id.to_owned(), room_id.to_owned());

			send(services, base, request).await
		},
		| Self::EventAuth => {
			let request = EventAuthRequest::new(room_id.to_owned(), event_id.to_owned());

			send(services, base, request).await
		},
	}
}

/// The auth chain served with a room's state carries what that state names.
///
/// A rename replaces the member's join in the state, while the rename still
/// names that join among its auth events; only the auth chain can carry the
/// join to a peer. The requested event is the owner's, so its own auth chain
/// never reaches the member's join.
async fn auth_chain_closes_over_state(
	owner: &Client<'_>,
	member: &Client<'_>,
	member_id: &UserId,
	version: &str,
) -> Result {
	let services = owner.services;
	let room_id = owner
		.create_room(&json!({ "preset": "public_chat", "room_version": version }))
		.await?;

	member
		.post(&format!("rooms/{room_id}/join"), &json!({}))
		.await?;

	let renamed = rename_member(member, &room_id, member_id).await?;

	let txn = format!("closure-{version}");
	let sent = owner
		.send_text(&room_id, &txn, "After the rename")
		.await?;

	let room_version = services.state.get_room_version(&room_id).await?;

	for route in [Route::State, Route::StateIds] {
		let body = route
			.get_ok(services, owner.base, &room_id, &sent)
			.await?;

		let served = route
			.served(services, &room_version, &body)
			.await?;

		let missing: Vec<_> = unserved(&served).collect();

		assert!(served.contains_key(&renamed), "{route:?} in v{version} omits the rename");
		assert!(missing.is_empty(), "{route:?} in v{version} omits auth events {missing:?}");
	}

	Ok(())
}

async fn rename_member(
	member: &Client<'_>,
	room_id: &RoomId,
	member_id: &UserId,
) -> Result<OwnedEventId> {
	let rename = json!({ "membership": "join", "displayname": "Renamed member" });
	let path = format!("rooms/{room_id}/state/m.room.member/{member_id}");

	member.put_event(&path, &rename).await
}

/// An event that storage cannot read drops out of each response that serves it.
///
/// The topic is served only as state, and the member's first join only in the
/// auth chain once a rename replaces it. With both unreadable, `/state` and
/// `send_join` serve every other event, and `/state_ids`, which reads none,
/// still names both.
async fn unreadable_events_are_omitted(
	owner: &Client<'_>,
	member: &Client<'_>,
	member_id: &UserId,
	joiner_id: &UserId,
) -> Result {
	let services = owner.services;
	let room_id = owner
		.create_room(&json!({ "preset": "public_chat", "topic": "Unreadable" }))
		.await?;

	member
		.post(&format!("rooms/{room_id}/join"), &json!({}))
		.await?;

	let joined =
		state_id(services, &room_id, &StateEventType::RoomMember, member_id.as_str()).await?;

	let renamed = rename_member(member, &room_id, member_id).await?;

	let sent = owner
		.send_text(&room_id, "unreadable", "Before the reads fail")
		.await?;

	let topic = state_id(services, &room_id, &StateEventType::RoomTopic, "").await?;
	let room_version = services.state.get_room_version(&room_id).await?;

	corrupt(services, &topic).await?;
	corrupt(services, &joined).await?;

	let body = Route::State
		.get_ok(services, owner.base, &room_id, &sent)
		.await?;

	let state_served = Route::State
		.served(services, &room_version, &body)
		.await?;

	let body = Route::StateIds
		.get_ok(services, owner.base, &room_id, &sent)
		.await?;

	let named: Vec<OwnedEventId> = serde_json::from_str::<Fields<'_, _>>(&body)?
		.into_values()
		.flatten()
		.collect();

	assert!(named.contains(&topic), "/state_ids omits the topic");
	assert!(named.contains(&joined), "/state_ids omits the first join");

	let join = prepare(services, joiner_id, &room_id, MembershipState::Join).await?;
	let (status, body) =
		submit(services, owner.base, JoinRequest::new, &room_id, &join.event_id, join.pdu)
			.await?;

	assert_eq!(status, 200, "send_join: {body}");

	let send_join_served = join_served(&room_version, &body)?;

	for (route, served) in [("/state", &state_served), ("send_join", &send_join_served)] {
		let missing: Vec<_> = unserved(served).map(at!(1)).collect();

		assert!(served.contains_key(&renamed), "{route} omits the rename");
		assert!(!served.contains_key(&topic), "{route} serves the unreadable topic");
		assert_eq!(missing, [&joined], "{route} omits more than the unreadable join");
	}

	Ok(())
}

/// A room whose auth chain cannot be walked is served without one.
///
/// Its create event is rewritten to name a room version this server does not
/// support, which fails every walk of the room's auth chain. Each route still
/// answers, with the state it serves and an empty auth chain.
async fn failed_walk_omits_the_auth_chain(owner: &Client<'_>) -> Result {
	let services = owner.services;
	let room_id = owner.create_room(&json!({})).await?;
	let sent = owner
		.send_text(&room_id, "walk", "Before the walk fails")
		.await?;

	let create = state_id(services, &room_id, &StateEventType::RoomCreate, "").await?;
	let pdu_id = services.timeline.get_pdu_id(&create).await?;
	let unsupported = services
		.timeline
		.get_pdu_json(&create)
		.await
		.and_then(unsupported_version)?;

	services
		.timeline
		.replace_pdu(&pdu_id, &unsupported)
		.await?;

	for route in ROUTES {
		let body = route
			.get_ok(services, owner.base, &room_id, &sent)
			.await?;

		for (list, events) in serde_json::from_str::<Fields<'_, Value>>(&body)? {
			let is_auth_chain = list.starts_with("auth_chain");

			assert_eq!(events.is_empty(), is_auth_chain, "{route:?} serves {list}: {events:?}");
		}
	}

	Ok(())
}

/// Overwrite an event's stored rows with bytes no read can decode.
///
/// Both the accepted row and an outlier row hold them, so every read of the
/// event fails as unreadable rather than as missing.
async fn corrupt(services: &Services, event_id: &EventId) -> Result {
	let pdu_id = services.timeline.get_pdu_id(event_id).await?;

	services.db["pduid_pdu"].insert(&pdu_id, UNREADABLE);
	services.db["eventid_outlierpdu"].insert(event_id, UNREADABLE);

	Ok(())
}

/// The events a `send_join` response serves, each with the auth events it
/// names.
///
/// The state and the auth chain merge into one map, as `/state`'s two lists
/// do, and a served PDU whose ID cannot be derived fails the test.
fn join_served(room_version: &RoomVersionId, body: &str) -> Result<Served> {
	let JoinState { state, auth_chain } = serde_json::from_str(body)?;

	state
		.into_iter()
		.chain(auth_chain)
		.map(|pdu| served_pdu(pdu, room_version))
		.collect()
}

/// A create event's JSON, changed to name an unsupported room version.
///
/// A create event without a content object fails the test instead.
fn unsupported_version(mut create: CanonicalJsonObject) -> Result<CanonicalJsonObject> {
	create
		.get_mut("content")
		.and_then(CanonicalJsonValue::as_object_mut)
		.ok_or_else(|| err!("create event has no content object"))?
		.insert("room_version".into(), CanonicalJsonValue::String(UNSUPPORTED_VERSION.into()));

	Ok(create)
}

/// The events a response body serves, each with the auth events it names.
///
/// A served PDU's id is derived as a peer derives it; a served id has its
/// event read back from the timeline.
#[implement(Route)]
async fn served(
	self,
	services: &Services,
	room_version: &RoomVersionId,
	body: &str,
) -> Result<Served> {
	match self {
		| Self::State | Self::EventAuth =>
			serde_json::from_str::<Fields<'_, CanonicalJsonObject>>(body)?
				.into_values()
				.flatten()
				.map(|pdu| served_pdu(pdu, room_version))
				.collect(),
		| Self::StateIds =>
			serde_json::from_str::<Fields<'_, OwnedEventId>>(body)?
				.into_values()
				.flatten()
				.try_stream()
				.broad_and_then(|event_id| served_id(services, event_id))
				.try_collect()
				.await,
	}
}

fn served_pdu(pdu: CanonicalJsonObject, room_version: &RoomVersionId) -> Result<ServedEvent> {
	let event_id = gen_event_id(&pdu, room_version)?;
	let auth_events = pdu
		.into_iter()
		.find_map(|(key, value)| key.eq("auth_events").then_some(value))
		.ok_or_else(|| err!("served PDU {event_id} omitted auth_events"))?;

	Ok((event_id, serde_json::from_value(auth_events.into())?))
}

async fn served_id(services: &Services, event_id: OwnedEventId) -> Result<ServedEvent> {
	let pdu = services.timeline.get_pdu(&event_id).await?;
	let auth_events = pdu.auth_events().map(ToOwned::to_owned).collect();

	Ok((event_id, auth_events))
}

/// Each auth event a response names without serving it.
///
/// A pair holds the served event that names the auth event, then the auth
/// event itself.
fn unserved(served: &Served) -> impl Iterator<Item = (&OwnedEventId, &OwnedEventId)> {
	served
		.iter()
		.flat_map(move |(event_id, auth_events)| {
			auth_events
				.iter()
				.filter(move |auth_event| !served.contains_key(*auth_event))
				.map(move |auth_event| (event_id, auth_event))
		})
}

async fn state_id(
	services: &Services,
	room_id: &RoomId,
	event_type: &StateEventType,
	state_key: &str,
) -> Result<OwnedEventId> {
	services
		.state_accessor
		.room_state_get_id(room_id, event_type, state_key)
		.await
}
