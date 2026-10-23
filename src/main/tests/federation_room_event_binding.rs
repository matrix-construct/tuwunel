#![cfg(test)]

use std::collections::BTreeMap;

use futures::TryStreamExt;
use serde_json::json;
use tuwunel_core::{
	Result, err, implement,
	ruma::{
		CanonicalJsonObject, EventId, OwnedEventId, RoomId, RoomVersionId, UserId,
		api::{
			OutgoingRequest, OutgoingRequestExt,
			federation::{
				authentication::{ServerSignatures, ServerSignaturesInput},
				authorization::get_event_authorization::v1::Request as EventAuthRequest,
				event::{
					get_room_state::v1::Request as StateRequest,
					get_room_state_ids::v1::Request as StateIdsRequest,
				},
			},
			path_builder::SinglePath,
		},
		events::StateEventType,
	},
	utils::stream::{IterStream, TryBroadbandExt},
};
use tuwunel_matrix::{Event, event::gen_event_id};
use tuwunel_service::Services;

use self::{
	client::{Client, field, register},
	fixture::boot,
};

mod client;
mod fixture;

#[path = "client/put.rs"]
mod put;

const ROUTES: [Route; 3] = [Route::State, Route::StateIds, Route::EventAuth];

const OWNER_TOKEN: &str = "room-event-binding-owner-access-token";
const MEMBER_TOKEN: &str = "room-event-binding-member-access-token";

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

/// The federation `/state`, `/state_ids` and `/event_auth` routes answer for
/// the requested room alone.
///
/// An event from another room is as unknown as one that does not exist. The
/// auth chain beside a room's state carries every auth event that state
/// names, so a peer holding nothing else can authorise each state event.
#[test]
fn federation_routes_bind_events_to_rooms() -> Result { boot("room-event-binding", [], exercise) }

async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "binding", OWNER_TOKEN).await?;

	let member_id = register(services, "bindingmember", MEMBER_TOKEN).await?;
	let owner = Client { services, base, token: OWNER_TOKEN };
	let member = Client { services, base, token: MEMBER_TOKEN };

	let room = owner.create_room(&json!({})).await?;
	let event = state_id(services, &room, &StateEventType::RoomGuestAccess).await?;
	let room_v12 = owner
		.create_room(&json!({ "room_version": "12" }))
		.await?;

	let event_v12 = state_id(services, &room_v12, &StateEventType::RoomGuestAccess).await?;
	let create_v12 = state_id(services, &room_v12, &StateEventType::RoomCreate).await?;
	let unknown = EventId::parse("$room-event-binding-unknown")?;

	for route in ROUTES {
		let response = route.get(services, base, &room, &event).await?;

		assert_eq!(response.0, 200, "{route:?}: {}", response.1);

		let foreign = route
			.get(services, base, &room, &event_v12)
			.await?;

		let unknown = route.get(services, base, &room, &unknown).await?;

		assert_eq!(foreign.0, 404);
		assert_eq!(unknown, foreign);

		let response_v12 = route
			.get(services, base, &room_v12, &event_v12)
			.await?;

		assert_eq!(response_v12.0, 200, "{route:?}: {}", response_v12.1);
	}

	let create_response = Route::EventAuth
		.get(services, base, &room_v12, &create_v12)
		.await?;

	assert_eq!(create_response.0, 200, "v12 create event: {}", create_response.1);

	for version in ["11", "12"] {
		auth_chain_closes_over_state(&owner, &member, &member_id, version).await?;
	}

	Ok(())
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

async fn send<T>(services: &Services, base: &str, request: T) -> Result<(u16, String)>
where
	T: OutgoingRequest<Authentication = ServerSignatures, PathBuilder = SinglePath>,
{
	let server_name = services.globals.server_name().to_owned();
	let auth = ServerSignaturesInput::new(
		server_name.clone(),
		server_name,
		services.server_keys.keypair(),
	);
	let request = request.try_into_http_request::<Vec<u8>>(base, auth, ())?;
	let response = services
		.client
		.clients
		.default
		.execute(request.try_into()?)
		.await?;

	let status = response.status().as_u16();
	let body = response.text().await?;

	Ok((status, body))
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

	let rename = json!({ "membership": "join", "displayname": "Renamed member" });
	let renamed = member
		.put(&format!("rooms/{room_id}/state/m.room.member/{member_id}"), &rename)
		.await?;

	let message = json!({ "msgtype": "m.text", "body": "After the rename" });
	let sent = owner
		.put(&format!("rooms/{room_id}/send/m.room.message/closure-{version}"), &message)
		.await?;

	let renamed = EventId::parse(field(&renamed, "event_id")?)?;
	let sent = EventId::parse(field(&sent, "event_id")?)?;
	let room_version = services.state.get_room_version(&room_id).await?;

	for route in [Route::State, Route::StateIds] {
		let (status, body) = route
			.get(services, owner.base, &room_id, &sent)
			.await?;

		assert_eq!(status, 200, "{route:?} in v{version}: {body}");

		let served = route
			.served(services, &room_version, &body)
			.await?;

		let missing: Vec<_> = unserved(&served).collect();

		assert!(served.contains_key(&renamed), "{route:?} in v{version} omits the rename");
		assert!(missing.is_empty(), "{route:?} in v{version} omits auth events {missing:?}");
	}

	Ok(())
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
) -> Result<OwnedEventId> {
	services
		.state_accessor
		.room_state_get_id(room_id, event_type, "")
		.await
}
