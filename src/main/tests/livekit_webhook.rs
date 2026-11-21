#![cfg(test)]
#![cfg(feature = "livekit")]

//! Tests LiveKit's webhooks (MSC4195) on a booted server.
//!
//! A stand-in LiveKit creates rooms, answers GetParticipant as each case sets
//! it and removes participants, and each case signs webhooks as LiveKit does.
//! A webhook that does not verify is refused before anything reaches LiveKit.
//! A participant reported joining is removed if its user has left the room,
//! and a delegated participant reported joining, leaving or aborting its
//! connection is followed before the webhook is answered, ahead of the next
//! scheduled poll. Polls of one participant run one at a time, and a notice
//! about no participant is acknowledged.

use std::{iter::from_fn, net::TcpListener, time::Duration};

use axum::{
	Json, Router,
	body::to_bytes,
	extract::{Request, State},
	http::{StatusCode, request::Parts},
	response::{IntoResponse, Response as AxumResponse},
};
use axum_server::from_tcp;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::future::join;
use reqwest::{Method, Response};
use serde_json::{Value, from_slice, json};
use tokio::{
	sync::{
		mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
		watch::{Receiver, Sender, channel},
	},
	task::JoinSet,
	time::{sleep, timeout},
};
use tuwunel_core::{
	Result, err,
	http::header::{AUTHORIZATION, CONTENT_TYPE},
	implement,
	jwt::{Algorithm, EncodingKey, Header, encode},
	ruma::{
		OwnedUserId, RoomId, UserId,
		api::client::delayed_events::get_delayed_event::v1::Finalized,
	},
	utils::{hash::sha256::hash as sha256, time::now_secs},
};
use tuwunel_service::{Services, delayed_events::Event};

use self::{
	client::{Client, accepted, field, register},
	common::{API_KEY, FOLDED_SLOT, SECRET, SLOT, claims, derive, refusal},
	fixture::boot,
};

mod client;
mod fixture;

#[expect(
	dead_code,
	reason = "The request editor serves the other LiveKit tests."
)]
#[path = "livekit/common.rs"]
mod common;

#[path = "client/membership.rs"]
mod membership;

const OWNER_TOKEN: &str = "livekit-webhook-owner-token-00001";
const STAYS_TOKEN: &str = "livekit-webhook-stays-token-00001";
const LEAVER_TOKEN: &str = "livekit-webhook-leaver-token-0001";

const MEMBER_ID: &str = "memberABC";
const CALL_MEMBER: &str = "org.matrix.msc3401.call.member";
const WEBHOOK: &str = "/_tuwunel/livekit/webhook";
const CREATE_ROOM: &str = "/twirp/livekit.RoomService/CreateRoom";
const GET_PARTICIPANT: &str = "/twirp/livekit.RoomService/GetParticipant";
const REMOVE_PARTICIPANT: &str = "/twirp/livekit.RoomService/RemoveParticipant";
const HOUR_MS: u64 = 3_600_000;

const KICK_DEADLINE: Duration = Duration::from_secs(10);

// Ample for a second webhook to reach LiveKit, were it not held back.
const GRACE: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Eq, PartialEq)]
enum Presence {
	/// LiveKit reports the participant connected, with media flowing.
	Active,

	/// Twirp's `not_found`: LiveKit knows no such participant.
	Unknown,

	/// The stand-in answers nothing until the case sets another presence, and
	/// then reports that one.
	Hold,
}

#[derive(Clone)]
struct StandIn {
	requests: UnboundedSender<String>,
	removals: UnboundedSender<Removal>,
	presence: Receiver<Presence>,
}

struct Case<'a> {
	services: &'a Services,
	base: &'a str,
	owner: Client<'a>,
	owner_id: &'a UserId,
	room_id: &'a RoomId,
	url: &'a str,
	set_presence: &'a Sender<Presence>,
}

struct Member<'a> {
	client: Client<'a>,
	user_id: OwnedUserId,
}

struct Removal {
	authorization: String,
	body: Value,
}

#[test]
fn receives_webhooks() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let url = format!("ws://{}", listener.local_addr()?);
	let options = [
		format!(r#"livekit.url="{url}""#),
		format!(r#"livekit.api_key="{API_KEY}""#),
		format!(r#"livekit.api_secret="{SECRET}""#),
	];

	boot(
		"livekit-webhook",
		options.iter().map(String::as_str),
		async |services: &Services, base: &str| exercise(services, base, listener, &url).await,
	)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, base: &str, listener: TcpListener, url: &str) -> Result {
	listener.set_nonblocking(true)?;

	let (requests, mut asked) = unbounded_channel();
	let (removals, mut seen) = unbounded_channel();
	let (set_presence, presence) = channel(Presence::Unknown);
	let app = Router::new()
		.fallback(stand_in)
		.with_state(StandIn { requests, removals, presence });

	let serving = from_tcp(listener)?.serve(app.into_make_service());
	let mut tasks = JoinSet::new(); // The set aborts the stand-in when the exercise exits.

	tasks.spawn_on(serving, services.server.runtime());

	let owner_id = register(services, "lkowner", OWNER_TOKEN).await?;
	let owner = Client { services, base, token: OWNER_TOKEN };
	let room_id = owner.create_room(&json!({})).await?;
	let case = Case {
		services,
		base,
		owner,
		owner_id: &owner_id,
		room_id: &room_id,
		url,
		set_presence: &set_presence,
	};

	let stays = case.member("lkstays", STAYS_TOKEN).await?;
	let leaver = case.member("lkleaver", LEAVER_TOKEN).await?;
	let jwt = case.issue(&stays).await?;

	case.issue(&leaver).await?;
	leaver.client.act(case.room_id, "leave").await?;

	// The kick on leaving goes first, so the cases see only what a webhook asks.
	next_removal(&mut seen).await?;
	drain(&mut asked);

	refuses_what_does_not_verify(&case, &leaver, &jwt, &mut asked).await?;
	joined_revalidates(&case, &stays, &leaver, &mut seen).await?;
	follows_delegations_at_once(&case, "joined-then-left", "participant_left").await?;
	follows_delegations_at_once(&case, "joined-then-aborted", "participant_connection_aborted")
		.await?;

	polls_of_one_participant_run_in_order(&case, &mut asked).await
}

/// A webhook without LiveKit's signature is unauthorized, and one whose body
/// LiveKit could not have sent is a bad request; neither reaches LiveKit.
///
/// A missing header, a token under another secret and a join token each send
/// the departed member's participant joining, which a verified webhook would
/// have removed. A signed body that is not JSON is the bad request.
#[tracing::instrument(level = "debug", skip_all)]
async fn refuses_what_does_not_verify(
	case: &Case<'_>,
	leaver: &Member<'_>,
	jwt: &str,
	asked: &mut UnboundedReceiver<String>,
) -> Result {
	let body = case.notice("participant_joined", &leaver.user_id, MEMBER_ID);
	let forged = token(&body, "another-secret");

	for authorization in [None, Some(forged.as_str()), Some(jwt)] {
		let response = case.notify(authorization, &body).await?;

		assert_eq!(refusal(response).await?, (401, "M_UNAUTHORIZED".into()));
	}

	let garbled = b"not json";
	let response = case
		.notify(Some(&token(garbled, SECRET)), garbled)
		.await?;

	assert_eq!(refusal(response).await?, (400, "M_BAD_JSON".into()));
	assert!(asked.try_recv().is_err(), "a refused webhook reached LiveKit");

	Ok(())
}

/// A participant reported joining is removed once its user has left the room,
/// and kept while its user stays joined.
///
/// The webhook is answered once its removal is made, with an empty object. A
/// participant this server never issued a token for, an event this server
/// does not follow, and one about a room with no name are acknowledged and
/// left alone.
#[tracing::instrument(level = "debug", skip_all)]
async fn joined_revalidates(
	case: &Case<'_>,
	stays: &Member<'_>,
	leaver: &Member<'_>,
	seen: &mut UnboundedReceiver<Removal>,
) -> Result {
	let since = now_secs();
	let joined = case.notice("participant_joined", &leaver.user_id, MEMBER_ID);

	assert_eq!(case.notified(&joined).await?, json!({}));

	let removal = seen
		.try_recv()
		.map_err(|e| err!("a departed user's participant was not removed: {e}"))?;

	assert_eq!(removed(&removal, since)?, case.participant(&leaver.user_id, MEMBER_ID));

	for body in [
		case.notice("participant_joined", &stays.user_id, MEMBER_ID),
		case.notice("participant_joined", &leaver.user_id, "never-issued"),
		case.notice("room_started", &leaver.user_id, MEMBER_ID),
	] {
		assert_eq!(case.notified(&body).await?, json!({}));
		assert!(seen.try_recv().is_err(), "LiveKit was asked to remove a participant");
	}

	// LiveKit leaves out the name of an egress's room, which has none.
	let egress =
		br#"{"event":"egress_started","room":{},"egressInfo":{"egressId":"EG_stand_in"}}"#;

	assert_eq!(case.notified(egress).await?, json!({}));

	Ok(())
}

/// A delegated participant reported joining is connected, and one then
/// reported gone has its delayed leave event sent, each before the webhook is
/// answered.
///
/// LiveKit reports the participant active for the first webhook and no longer
/// knows it for the second, and the case runs no poll of its own.
#[tracing::instrument(level = "debug", skip_all)]
async fn follows_delegations_at_once(case: &Case<'_>, member: &str, departure: &str) -> Result {
	let delay_id = case.schedule(member).await?;

	case.delegate(member, &delay_id).await?;
	assert!(!case.connected(member).await?, "a new delegation began connected");

	case.set_presence.send_replace(Presence::Active);

	let joined = case.notice("participant_joined", case.owner_id, member);

	case.notified(&joined).await?;
	assert!(case.connected(member).await?, "{member} was not connected");

	let pending = case.event(&delay_id).await?.finalised.is_none();
	let left = case.notice(departure, case.owner_id, member);

	assert!(pending, "{member}'s event ended while connected");

	// Gone only from here, so a worker poll could send the event only during the webhook's call.
	case.set_presence.send_replace(Presence::Unknown);
	case.notified(&left).await?;

	let finalised = case.event(&delay_id).await?.finalised;

	assert!(matches!(finalised, Some(Finalized::Sent { .. })), "{member}: {finalised:?}");

	Ok(())
}

/// Polls of one delegation run one at a time, so LiveKit's answers act in the
/// order they were asked.
///
/// While LiveKit holds its first answer, webhooks reporting the participant
/// gone and joined arrive together and LiveKit is asked about it once. Polls
/// in order mean an earlier poll's departure never overtakes a reconnect a
/// later poll saw. Once LiveKit answers that the participant is gone, the
/// event is sent and both webhooks are acknowledged.
#[tracing::instrument(level = "debug", skip_all)]
async fn polls_of_one_participant_run_in_order(
	case: &Case<'_>,
	asked: &mut UnboundedReceiver<String>,
) -> Result {
	let member = "ordered";
	let delay_id = case.schedule(member).await?;
	let joined = case.notice("participant_joined", case.owner_id, member);
	let left = case.notice("participant_left", case.owner_id, member);

	case.delegate(member, &delay_id).await?;
	case.set_presence.send_replace(Presence::Active);
	case.notified(&joined).await?;
	assert!(case.connected(member).await?, "{member} was not connected");

	drain(asked);
	case.set_presence.send_replace(Presence::Hold);

	let webhooks = join(case.notified(&left), case.notified(&joined));
	let ((left, joined), asks) = join(webhooks, asks_while_held(case, asked)).await;

	assert_eq!((left?, joined?), (json!({}), json!({})));
	assert_eq!(asks?, 1, "LiveKit was asked again before it answered");

	let finalised = case.event(&delay_id).await?.finalised;

	assert!(matches!(finalised, Some(Finalized::Sent { .. })), "{finalised:?}");

	Ok(())
}

/// Counts LiveKit's requests while its answer is held, then has it answer
/// that the participant is gone.
///
/// The first request is awaited, and every other that arrives within the
/// grace period is counted with it.
#[tracing::instrument(level = "debug", skip_all)]
async fn asks_while_held(
	case: &Case<'_>,
	asked: &mut UnboundedReceiver<String>,
) -> Result<usize> {
	let first = timeout(KICK_DEADLINE, asked.recv())
		.await
		.ok()
		.flatten()
		.ok_or_else(|| err!("LiveKit was asked about no participant"))?;

	assert_eq!(first, GET_PARTICIPANT);
	sleep(GRACE).await;

	let asks = drain(asked);

	case.set_presence.send_replace(Presence::Unknown);

	Ok(asks.saturating_add(1))
}

// Takes every request already recorded, and counts them.
fn drain(asked: &mut UnboundedReceiver<String>) -> usize {
	from_fn(|| asked.try_recv().ok()).count()
}

/// Registers a user and joins it to the room.
///
/// The owner invites it and it accepts through the client API, so its
/// membership is the one the server's own checks read.
#[implement(Case, generics = "<'a>", params = "<'a>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn member(&self, localpart: &str, token: &'a str) -> Result<Member<'a>> {
	let user_id = register(self.services, localpart, token).await?;
	let client = Client {
		services: self.services,
		base: self.base,
		token,
	};

	self.owner
		.admit(self.room_id, &client, &user_id)
		.await?;

	Ok(Member { client, user_id })
}

/// Issues a member a token for the call slot, and returns it.
///
/// The request goes through the client API, so the server records the
/// participant as it does for any client.
#[implement(Case, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn issue(&self, member: &Member<'_>) -> Result<String> {
	let body = json!({
		"url": self.url,
		"room_id": self.room_id,
		"slot_id": SLOT,
		"member_id": MEMBER_ID,
	});

	let url = member
		.client
		.client_url("v1/", "rtc/livekit/get_token");

	let reply = member.client.post_json(&url, &body).await?;

	Ok(field(&reply, "jwt")?.to_owned())
}

/// A webhook body as LiveKit writes it, reporting an event about a user's
/// participant in the call slot.
///
/// The room and identity are derived here by MSC4195's construction, apart
/// from the server's own code.
#[implement(Case, params = "<'_>")]
fn notice(&self, event: &str, user_id: &UserId, member_id: &str) -> Vec<u8> {
	let (room, identity) = self.participant(user_id, member_id);
	let notice = json!({
		"event": event,
		"room": { "sid": "RM_stand_in", "name": room },
		"participant": { "sid": "PA_stand_in", "identity": identity, "state": "ACTIVE" },
		"id": "EV_stand_in",
		"createdAt": now_secs().to_string(),
	});

	serde_json::to_vec(&notice).expect("a JSON value serializes")
}

/// Sends LiveKit's signed webhook and returns the server's answer, which
/// must be a success.
///
/// The token is signed as LiveKit signs it, over these exact bytes.
#[implement(Case, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn notified(&self, body: &[u8]) -> Result<Value> {
	let response = self
		.notify(Some(&token(body, SECRET)), body)
		.await?;

	accepted(response).await
}

/// Posts a webhook as LiveKit does, with `authorization` as the raw token.
///
/// The value goes out verbatim, or not at all, so a case can send a missing
/// or invalid credential.
#[implement(Case, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn notify(&self, authorization: Option<&str>, body: &[u8]) -> Result<Response> {
	let request = self
		.services
		.client
		.clients
		.default
		.post(format!("{}{WEBHOOK}", self.base))
		.header(CONTENT_TYPE, "application/webhook+json")
		.body(body.to_owned());

	let request = match authorization {
		| Some(token) => request.header(AUTHORIZATION, token),
		| None => request,
	};

	Ok(request.send().await?)
}

/// The LiveKit room and identity of a user's participant in the call slot.
///
/// Both are derived here by MSC4195's construction, apart from the server's
/// own code.
#[implement(Case, params = "<'_>")]
fn participant(&self, user_id: &UserId, member_id: &str) -> (String, String) {
	(derive(self.room_id.as_str(), FOLDED_SLOT), derive(user_id.as_str(), member_id))
}

/// Schedules the owner's delayed call membership event for a member, and
/// returns its `delay_id`.
///
/// The event's content is empty, since only its `delay_id` matters to a
/// delegation.
#[implement(Case, params = "<'_>")]
#[tracing::instrument(level = "debug", skip(self))]
async fn schedule(&self, member: &str) -> Result<String> {
	let room_id = self.room_id;
	let path = format!(
		"rooms/{room_id}/state/{CALL_MEMBER}/{member}?org.matrix.msc4140.delay={HOUR_MS}"
	);

	let response = self
		.owner
		.request(Method::PUT, &self.owner.url(&path), &json!({}))
		.await?;

	let reply = accepted(response).await?;

	Ok(field(&reply, "delay_id")?.to_owned())
}

#[implement(Case, params = "<'_>")]
#[tracing::instrument(level = "debug", skip(self))]
async fn delegate(&self, member: &str, delay_id: &str) -> Result {
	let body = json!({
		"url": self.url,
		"room_id": self.room_id,
		"slot_id": SLOT,
		"member_id": member,
		"delay_id": delay_id,
	});

	let url = self
		.owner
		.client_url("v1/", "rtc/livekit/delegate_delayed_leave");

	self.owner.post_json(&url, &body).await.map(drop)
}

/// Whether the owner's delegation for a member is stored as connected.
///
/// The key is derived here by MSC4195's construction, so a delegation stored
/// under any other room or identity is an error.
#[implement(Case, params = "<'_>")]
#[tracing::instrument(level = "debug", skip(self))]
async fn connected(&self, member: &str) -> Result<bool> {
	let (room, identity) = self.participant(self.owner_id, member);
	let row = self.services.db["lkparticipant_delegation"]
		.qry(&(room.as_str(), identity.as_str()))
		.await?;

	let delegation: Value = from_slice(&row)?;
	let connected = delegation["connected"]
		.as_bool()
		.unwrap_or_default();

	Ok(connected)
}

#[implement(Case, params = "<'_>")]
#[tracing::instrument(level = "debug", skip(self))]
async fn event(&self, delay_id: &str) -> Result<Event> {
	self.services
		.delayed_events
		.get(delay_id, None)
		.await
}

/// The stand-in LiveKit: creates rooms, reports participants as the case set
/// and removes them.
///
/// Every request's path, and each removal whole, is handed to the case before
/// it is answered, so a case finds it once the server's call has returned.
/// While the presence is held, a GetParticipant waits for the next presence
/// and reports it.
#[tracing::instrument(level = "debug", skip_all)]
async fn stand_in(State(stand_in): State<StandIn>, request: Request) -> AxumResponse {
	let (parts, body) = request.into_parts();
	let bytes = to_bytes(body, usize::MAX)
		.await
		.unwrap_or_default();

	let body: Value = from_slice(&bytes).unwrap_or_default();
	let path = parts.uri.path();

	stand_in.requests.send(path.to_owned()).ok();

	match path {
		| CREATE_ROOM => Json(json!({ "name": body["name"] })).into_response(),
		| GET_PARTICIPANT => {
			let presence = stand_in
				.presence
				.clone()
				.wait_for(|presence| *presence != Presence::Hold)
				.await
				.map_or(Presence::Unknown, |presence| *presence);

			reported(presence, &body)
		},
		| REMOVE_PARTICIPANT => {
			record_removal(&stand_in.removals, &parts, body);
			Json(json!({})).into_response()
		},
		| _ => StatusCode::NOT_FOUND.into_response(),
	}
}

// LiveKit answers for an unknown participant with Twirp's `not_found` at HTTP 404.
fn reported(presence: Presence, request: &Value) -> AxumResponse {
	match presence {
		| Presence::Active =>
			Json(json!({ "identity": request["identity"], "state": "ACTIVE" })).into_response(),
		| Presence::Unknown | Presence::Hold => {
			let error = json!({ "code": "not_found", "msg": "participant does not exist" });

			(StatusCode::NOT_FOUND, Json(error)).into_response()
		},
	}
}

// Hands a RemoveParticipant request to the case, before it is answered.
fn record_removal(removals: &UnboundedSender<Removal>, parts: &Parts, body: Value) {
	let authorization = parts
		.headers
		.get(AUTHORIZATION)
		.and_then(|value| value.to_str().ok())
		.unwrap_or_default()
		.to_owned();

	removals
		.send(Removal { authorization, body })
		.ok();
}

#[tracing::instrument(level = "debug", skip_all)]
async fn next_removal(seen: &mut UnboundedReceiver<Removal>) -> Result<Removal> {
	timeout(KICK_DEADLINE, seen.recv())
		.await
		.ok()
		.flatten()
		.ok_or_else(|| err!("LiveKit was asked to remove no participant"))
}

/// Checks a RemoveParticipant request and returns the room and identity it
/// names.
///
/// Its token must administer that room, and it must revoke the tokens issued
/// until the moment of the call, which falls between `since` and now.
fn removed(request: &Removal, since: u64) -> Result<(String, String)> {
	let bearer = request
		.authorization
		.strip_prefix("Bearer ")
		.ok_or_else(|| err!("no Bearer token on a RemoveParticipant"))?;

	let grant = &claims(bearer)?["video"];
	let room = field(&request.body, "room")?.to_owned();
	let identity = field(&request.body, "identity")?.to_owned();
	let revoked = request.body["revoke_token_ts"]
		.as_u64()
		.unwrap_or_default();

	assert_eq!((&grant["room"], &grant["roomAdmin"]), (&json!(room), &json!(true)), "{grant}");
	assert!((since..=now_secs()).contains(&revoked), "revoke_token_ts {revoked} is not now");

	Ok((room, identity))
}

/// Signs a webhook body as LiveKit does.
///
/// The token is HS256 under `secret`, issued by the API key for five minutes,
/// and carries the padded standard base64 SHA-256 of the body.
fn token(body: &[u8], secret: &str) -> String {
	let now = now_secs();
	let fields = json!({
		"iss": API_KEY,
		"nbf": now,
		"exp": now.saturating_add(300),
		"sha256": STANDARD.encode(sha256(body)),
	});

	let key = EncodingKey::from_secret(secret.as_bytes());

	encode(&Header::new(Algorithm::HS256), &fields, &key).expect("a webhook token signs")
}
