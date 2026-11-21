#![cfg(test)]
#![cfg(feature = "livekit")]

//! Delegated delayed leave events (MSC4195) on a booted server.
//!
//! A stand-in LiveKit answers GetParticipant as each case sets it, and each
//! case runs polls itself, as of the time it needs. A delegated event restarts
//! near its due time while its participant stays connected, goes out once the
//! participant leaves, and is left alone when the participant never connects,
//! LiveKit cannot answer, or the delegation names another SFU.

use std::{net::TcpListener, time::Duration};

use axum::{
	Json, Router,
	body::to_bytes,
	extract::{Request, State},
	http::StatusCode,
	response::{IntoResponse, Response as AxumResponse},
};
use axum_server::from_tcp;
use futures::future::join;
use reqwest::{Method, Response};
use serde_json::{Map, Value, from_slice, json};
use tokio::{
	sync::{
		mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
		watch::{Receiver, Sender, channel},
	},
	task::JoinSet,
	time::timeout,
};
use tuwunel_core::{
	Result, err,
	http::header::AUTHORIZATION,
	implement,
	ruma::{RoomId, UserId, api::client::delayed_events::get_delayed_event::v1::Finalized},
	utils::time::now_millis,
};
use tuwunel_service::{Services, delayed_events::Event};

use self::{
	client::{Client, accepted, field, register},
	common::{API_KEY, FOLDED_SLOT, SECRET, SLOT, claims, derive, refusal, with},
	fixture::boot,
};

mod client;
mod fixture;

#[path = "livekit/common.rs"]
mod common;

const CALLER_TOKEN: &str = "livekit-delegation-caller-token-01";
const OTHER_TOKEN: &str = "livekit-delegation-other-token-001";
const CALL_MEMBER: &str = "org.matrix.msc3401.call.member";
const DELEGATE: &str = "rtc/livekit/delegate_delayed_leave";
const GET_PARTICIPANT: &str = "/twirp/livekit.RoomService/GetParticipant";
const PREFIXES: [&str; 2] = ["v1/", "unstable/io.element.msc4195/"];
const HOUR_MS: u64 = 3_600_000;

const MIN_DELAY_MS: u64 = 60_000;

// Ample for one poll that asks LiveKit nothing, and short of a held answer.
const POLL_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Eq, PartialEq)]
enum Answer {
	/// A participant in this LiveKit state.
	State(&'static str),

	/// Twirp's `not_found`: LiveKit knows no such participant.
	Unknown,

	/// A 500 with a Twirp error.
	Failure,

	/// No answer until the case sets another, which is then given.
	Hold,
}

#[derive(Clone)]
struct StandIn {
	seen: UnboundedSender<Seen>,
	answer: Receiver<Answer>,
}

struct Case<'a> {
	services: &'a Services,
	caller: Client<'a>,
	user_id: &'a UserId,
	room_id: &'a RoomId,
	url: &'a str,
	answer: &'a Sender<Answer>,
}

struct Seen {
	path: String,
	authorization: String,
	body: Value,
}

#[test]
fn follows_delegated_leaves() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let url = format!("ws://{}", listener.local_addr()?);
	let options = [
		format!(r#"livekit.url="{url}""#),
		format!(r#"livekit.api_key="{API_KEY}""#),
		format!(r#"livekit.api_secret="{SECRET}""#),
	];

	boot(
		"livekit-delegation",
		options.iter().map(String::as_str),
		async |services: &Services, base: &str| exercise(services, base, listener, &url).await,
	)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, base: &str, listener: TcpListener, url: &str) -> Result {
	listener.set_nonblocking(true)?;

	let (tx, mut seen) = unbounded_channel();
	let (answer, answers) = channel(Answer::Unknown);
	let app = Router::new()
		.fallback(stand_in)
		.with_state(StandIn { seen: tx, answer: answers });

	let serving = from_tcp(listener)?.serve(app.into_make_service());
	let mut tasks = JoinSet::new(); // The set aborts the stand-in when the exercise exits.

	tasks.spawn_on(serving, services.server.runtime());

	let user_id = register(services, "lkcaller", CALLER_TOKEN).await?;

	register(services, "lkother", OTHER_TOKEN).await?;

	let client = |token| Client { services, base, token };
	let caller = client(CALLER_TOKEN);
	let room_id = caller.create_room(&json!({})).await?;
	let elsewhere = caller.create_room(&json!({})).await?;
	let case = Case {
		services,
		caller,
		user_id: &user_id,
		room_id: &room_id,
		url,
		answer: &answer,
	};

	refuses(&case, &client(OTHER_TOKEN), &elsewhere).await?;
	follows_the_participant(&case, &mut seen).await?;
	restarts_near_due(&case).await?;
	waiting_gives_up(&case).await?;
	cancelled_is_dropped_without_asking_livekit(&case).await?;
	redelegation_replaces(&case).await?;
	redelegation_during_a_step_survives(&case, &mut seen).await?;
	same_event_redelegated_during_a_step_starts_over(&case, &mut seen).await?;
	same_event_redelegated_stays_connected(&case).await?;
	another_sfus_delegation_is_let_go(&case, &mut seen).await?;
	unreadable_delegation_is_not_replaced(&case).await?;
	stale_gone_after_connecting_is_dropped(&case, &mut seen).await?;
	unreadable_event_keeps_the_delegation(&case).await
}

/// Each invalid authenticated request answers 400 `M_INVALID_PARAM` and
/// stores nothing.
///
/// Another user's `delay_id`, one in another room, a cancelled one, an
/// unknown one, a delay under a minute and a `url` other than this server's
/// SFU are refused. Without an access token the unstable path answers 401,
/// which Element Call's probe reads as supported.
#[tracing::instrument(level = "debug", skip_all)]
async fn refuses(case: &Case<'_>, other: &Client<'_>, elsewhere: &RoomId) -> Result {
	let member = "refused";
	let delay_id = case.schedule(member, HOUR_MS).await?;
	let cancelled = case.schedule(member, HOUR_MS).await?;
	let short = case.schedule(member, MIN_DELAY_MS - 1).await?;
	let body = case.body(member, &delay_id);

	case.cancel(&cancelled).await?;

	for (client, body) in [
		(other, body.clone()),
		(&case.caller, with(&body, "room_id", elsewhere.as_str())),
		(&case.caller, with(&body, "delay_id", cancelled.as_str())),
		(&case.caller, with(&body, "delay_id", "unknown")),
		(&case.caller, with(&body, "delay_id", short.as_str())),
		(&case.caller, with(&body, "url", "ws://elsewhere.invalid")),
	] {
		let response = client
			.post_url(&client.client_url(PREFIXES[0], DELEGATE), &body)
			.await?;

		assert_eq!(refusal(response).await?, (400, "M_INVALID_PARAM".into()), "{body}");
	}

	assert!(case.delegation(member).await.is_none(), "a refused delegation was stored");

	let anonymous = case
		.services
		.client
		.clients
		.default
		.post(case.caller.client_url(PREFIXES[1], DELEGATE))
		.json(&body)
		.send()
		.await?;

	assert_eq!(refusal(anonymous).await?, (401, "M_MISSING_TOKEN".into()));

	case.cancel(&delay_id).await?;
	case.cancel(&short).await
}

/// A delegation follows its participant: connected once LiveKit reports it
/// active, untouched while LiveKit cannot answer, and sent once LiveKit no
/// longer knows it.
///
/// LiveKit is asked about the member's derived identity in the call's derived
/// room. An event an hour from due is not restarted.
#[tracing::instrument(level = "debug", skip_all)]
async fn follows_the_participant(case: &Case<'_>, seen: &mut UnboundedReceiver<Seen>) -> Result {
	let member = "follows";
	let delay_id = case.schedule(member, HOUR_MS).await?;

	case.delegated(PREFIXES[0], member, &delay_id)
		.await?;

	assert_eq!(case.stored(member).await?, (delay_id.clone(), false));

	let due = case.event(&delay_id).await?.due_ms;

	drain(seen);
	case.answer.send_replace(Answer::State("ACTIVE"));
	case.poll(now_millis()).await;

	assert_eq!(case.stored(member).await?, (delay_id.clone(), true));
	assert_eq!(case.event(&delay_id).await?.due_ms, due, "an event an hour from due restarted");
	asked_about(case, seen, member)?;

	case.answer.send_replace(Answer::Failure);
	case.poll(now_millis()).await;

	assert_eq!(case.stored(member).await?, (delay_id.clone(), true));
	assert!(case.event(&delay_id).await?.finalised.is_none(), "an outage sent the event");

	case.answer.send_replace(Answer::Unknown);
	case.poll(now_millis()).await;

	let finalised = case.event(&delay_id).await?.finalised;

	assert!(matches!(finalised, Some(Finalized::Sent { .. })), "{finalised:?}");
	assert!(case.delegation(member).await.is_none(), "a sent delegation was kept");

	Ok(())
}

/// A connected delegation at the shortest delay restarts its event when a
/// poll finds it near due.
///
/// A poll finding it far from due, as one right after a restart does, leaves
/// it alone.
#[tracing::instrument(level = "debug", skip_all)]
async fn restarts_near_due(case: &Case<'_>) -> Result {
	let member = "restarts";
	let delay_id = case.schedule(member, MIN_DELAY_MS).await?;

	case.delegated(PREFIXES[0], member, &delay_id)
		.await?;

	let due = case.event(&delay_id).await?.due_ms;

	case.answer.send_replace(Answer::State("ACTIVE"));
	case.poll(now_millis()).await;

	assert_eq!(case.stored(member).await?, (delay_id.clone(), true));
	assert_eq!(case.event(&delay_id).await?.due_ms, due, "an event far from due restarted");

	case.poll(now_millis().saturating_add(50_000))
		.await;

	let restarted = case.event(&delay_id).await?.due_ms;

	assert!(restarted > due, "an event near due was not restarted");

	case.poll(now_millis()).await;

	assert_eq!(case.event(&delay_id).await?.due_ms, restarted, "a fresh restart restarted");

	case.cancel(&delay_id).await
}

/// A participant that never connects is let go once its wait is over.
///
/// Its event stays scheduled, to fire on its own time.
#[tracing::instrument(level = "debug", skip_all)]
async fn waiting_gives_up(case: &Case<'_>) -> Result {
	let member = "waits";
	let delay_id = case.schedule(member, MIN_DELAY_MS).await?;

	case.delegated(PREFIXES[0], member, &delay_id)
		.await?;

	case.answer.send_replace(Answer::State("JOINING"));
	case.poll(now_millis().saturating_add(61_000))
		.await;

	assert!(case.delegation(member).await.is_none(), "a delegation outlived its wait");
	assert!(case.event(&delay_id).await?.finalised.is_none(), "giving up sent the event");

	case.cancel(&delay_id).await
}

/// A delegation whose event was cancelled is dropped on the next poll
/// without asking LiveKit.
///
/// LiveKit holds every answer meanwhile, so a poll that asked would not end
/// in time.
#[tracing::instrument(level = "debug", skip_all)]
async fn cancelled_is_dropped_without_asking_livekit(case: &Case<'_>) -> Result {
	let member = "cancels";
	let delay_id = case.schedule(member, HOUR_MS).await?;

	case.delegated(PREFIXES[0], member, &delay_id)
		.await?;

	case.cancel(&delay_id).await?;
	case.answer.send_replace(Answer::Hold);

	let polled = timeout(POLL_DEADLINE, case.poll(now_millis())).await;

	case.answer.send_replace(Answer::Unknown);

	assert!(polled.is_ok(), "the poll asked LiveKit about a cancelled delegation");
	assert!(case.delegation(member).await.is_none(), "a cancelled delegation was kept");

	Ok(())
}

#[tracing::instrument(level = "debug", skip_all)]
async fn redelegation_replaces(case: &Case<'_>) -> Result {
	let member = "replaced";
	let first = case.schedule(member, HOUR_MS).await?;
	let second = case.schedule(member, HOUR_MS).await?;

	for (prefix, delay_id) in [(PREFIXES[0], &first), (PREFIXES[1], &second)] {
		case.delegated(prefix, member, delay_id).await?;
	}

	assert_eq!(case.stored(member).await?, (second.clone(), false));

	case.cancel(&first).await?;
	case.cancel(&second).await
}

/// A delegation replaced while a poll waits on LiveKit survives that poll.
///
/// The poll read the first delegation, connected, and LiveKit then reports
/// the participant gone. The poll acts on the delegation as it stands after
/// the wait, so neither event is sent and the second stays delegated; the
/// replacing request does not wait for LiveKit either.
#[tracing::instrument(level = "debug", skip_all)]
async fn redelegation_during_a_step_survives(
	case: &Case<'_>,
	seen: &mut UnboundedReceiver<Seen>,
) -> Result {
	let member = "raced";
	let first = case.schedule(member, HOUR_MS).await?;
	let second = case.schedule(member, HOUR_MS).await?;

	case.delegated(PREFIXES[0], member, &first)
		.await?;

	case.answer.send_replace(Answer::State("ACTIVE"));
	case.poll(now_millis()).await;

	assert_eq!(case.stored(member).await?, (first.clone(), true));

	drain(seen);
	case.answer.send_replace(Answer::Hold);

	let gone = Answer::Unknown;
	let raced = join(case.poll(now_millis()), redelegate(case, seen, member, &second, gone));
	let ((), redelegated) = timeout(POLL_DEADLINE, raced)
		.await
		.map_err(|e| err!("the poll or the new delegation waited on LiveKit: {e}"))?;

	redelegated?;

	assert_eq!(case.stored(member).await?, (second.clone(), false));
	assert!(case.event(&first).await?.finalised.is_none(), "the replaced event was sent");

	case.cancel(&first).await?;
	case.cancel(&second).await
}

/// A delegation made again for the same event while a poll waits on LiveKit
/// starts over.
///
/// LiveKit then reports the participant active, an answer the poll got for
/// the earlier delegation, so the new one must stay waiting rather than
/// count as connected.
#[tracing::instrument(level = "debug", skip_all)]
async fn same_event_redelegated_during_a_step_starts_over(
	case: &Case<'_>,
	seen: &mut UnboundedReceiver<Seen>,
) -> Result {
	let member = "again";
	let delay_id = case.schedule(member, HOUR_MS).await?;

	case.delegated(PREFIXES[0], member, &delay_id)
		.await?;

	drain(seen);
	case.answer.send_replace(Answer::Hold);

	let active = Answer::State("ACTIVE");
	let raced = join(case.poll(now_millis()), redelegate(case, seen, member, &delay_id, active));
	let ((), redelegated) = timeout(POLL_DEADLINE, raced)
		.await
		.map_err(|e| err!("the poll or the new delegation waited on LiveKit: {e}"))?;

	redelegated?;
	case.answer.send_replace(Answer::State("JOINING"));

	assert_eq!(case.stored(member).await?, (delay_id.clone(), false));

	case.cancel(&delay_id).await
}

/// A delegation made again for the same event keeps the connection recorded
/// for its participant, so the participant's departure still sends the event.
///
/// LiveKit reports the participant active, the client delegates the event
/// again, as Element Call does when its transport is prepared anew, and then
/// LiveKit no longer knows the participant.
#[tracing::instrument(level = "debug", skip_all)]
async fn same_event_redelegated_stays_connected(case: &Case<'_>) -> Result {
	let member = "kept";
	let delay_id = case.schedule(member, HOUR_MS).await?;

	case.delegated(PREFIXES[0], member, &delay_id)
		.await?;

	case.answer.send_replace(Answer::State("ACTIVE"));
	case.poll(now_millis()).await;
	case.delegated(PREFIXES[1], member, &delay_id)
		.await?;

	let (stored, connected) = case.stored(member).await?;

	assert_eq!((stored.as_str(), connected), (delay_id.as_str(), true));

	case.answer.send_replace(Answer::Unknown);
	case.poll(now_millis()).await;

	let finalised = case.event(&delay_id).await?.finalised;

	assert!(matches!(finalised, Some(Finalized::Sent { .. })), "{finalised:?}");
	assert!(case.delegation(member).await.is_none(), "a sent delegation was kept");

	Ok(())
}

/// A delegation made on another SFU, or stored before delegations named their
/// SFU, is let go without asking LiveKit, and its event is left to fire on its
/// own time.
///
/// Its stored `url` names another SFU, as a restart onto a new one leaves it,
/// or is missing. Only the SFU it names can tell whether its participant left,
/// so a poll here lets it go even when this SFU no longer knows the
/// participant. Delegated again on this SFU, it starts out waiting.
#[tracing::instrument(level = "debug", skip_all)]
async fn another_sfus_delegation_is_let_go(
	case: &Case<'_>,
	seen: &mut UnboundedReceiver<Seen>,
) -> Result {
	let member = "moved";
	let delay_id = case.schedule(member, HOUR_MS).await?;

	case.delegated(PREFIXES[0], member, &delay_id)
		.await?;

	let due = case.event(&delay_id).await?.due_ms;
	let record = case
		.delegation(member)
		.await
		.ok_or_else(|| err!("no delegation is stored for {member}"))?;

	let moved = with(&record, "url", "wss://previous.example.com");
	let moved = with(&moved, "connected", true);

	case.store(member, &moved);
	case.delegated(PREFIXES[0], member, &delay_id)
		.await?;

	let (stored, connected) = case.stored(member).await?;

	assert_eq!((stored.as_str(), connected), (delay_id.as_str(), false));

	let legacy = without(&moved, "url");

	drain(seen);
	case.answer.send_replace(Answer::Unknown);

	for stored in [&moved, &legacy] {
		case.store(member, stored);
		case.poll(now_millis()).await;

		assert!(case.delegation(member).await.is_none(), "{stored}: the delegation was kept");
		assert!(seen.try_recv().is_err(), "{stored}: LiveKit was asked about the participant");
	}

	let event = case.event(&delay_id).await?;

	assert!(event.finalised.is_none(), "letting go sent the event");
	assert_eq!(event.due_ms, due, "letting go moved the event");

	case.cancel(&delay_id).await
}

/// A delegation that cannot be read is not replaced, since what it recorded
/// of its participant would be lost.
///
/// The request fails with a 500, and the stored record is left as it was.
#[tracing::instrument(level = "debug", skip_all)]
async fn unreadable_delegation_is_not_replaced(case: &Case<'_>) -> Result {
	let member = "garbled";
	let delay_id = case.schedule(member, HOUR_MS).await?;

	case.delegated(PREFIXES[0], member, &delay_id)
		.await?;

	let record = case
		.delegation(member)
		.await
		.ok_or_else(|| err!("no delegation is stored for {member}"))?;

	let garbled = json!("not a delegation");

	case.store(member, &garbled);

	let response = case
		.delegate(PREFIXES[0], member, &delay_id)
		.await?;

	let (status, errcode) = refusal(response).await?;

	assert_eq!((status, errcode.as_str()), (500, "M_UNKNOWN"));
	assert_eq!(case.delegation(member).await, Some(garbled));

	case.store(member, &record);
	case.cancel(&delay_id).await
}

/// A poll's answer that the participant is gone is dropped when the
/// delegation was marked connected while the poll waited on LiveKit.
///
/// The poll comes after the connect wait, so its answer would let a waiting
/// delegation go, and applied to the connected one would send its leave. The
/// absence was observed before the connection, so it says nothing of the
/// participant now; the next poll asks again.
#[tracing::instrument(level = "debug", skip_all)]
async fn stale_gone_after_connecting_is_dropped(
	case: &Case<'_>,
	seen: &mut UnboundedReceiver<Seen>,
) -> Result {
	let member = "stale";
	let delay_id = case.schedule(member, HOUR_MS).await?;

	case.delegated(PREFIXES[0], member, &delay_id)
		.await?;

	drain(seen);
	case.answer.send_replace(Answer::Hold);

	let after_wait = now_millis().saturating_add(HOUR_MS);
	let raced = join(case.poll(after_wait), connect_meanwhile(case, seen, member));
	let ((), connected) = timeout(POLL_DEADLINE, raced)
		.await
		.map_err(|e| err!("the poll waited on LiveKit: {e}"))?;

	connected?;
	case.answer.send_replace(Answer::State("JOINING"));

	assert_eq!(case.stored(member).await?, (delay_id.clone(), true));
	assert!(
		case.event(&delay_id).await?.finalised.is_none(),
		"a stale absence sent the event"
	);

	case.cancel(&delay_id).await
}

/// Marks a member's delegation connected once LiveKit has been asked, as a
/// later poll would, then lets LiveKit answer that the participant is gone.
///
/// Waiting for the stand-in to see the request puts the connection between
/// the poll's question and its answer.
#[tracing::instrument(level = "debug", skip_all)]
async fn connect_meanwhile(
	case: &Case<'_>,
	seen: &mut UnboundedReceiver<Seen>,
	member: &str,
) -> Result {
	seen.recv()
		.await
		.ok_or_else(|| err!("the stand-in closed"))?;

	let record = case
		.delegation(member)
		.await
		.ok_or_else(|| err!("no delegation is stored for {member}"))?;

	case.store(member, &with(&record, "connected", true));
	case.answer.send_replace(Answer::Unknown);

	Ok(())
}

/// A delegation whose event cannot be read is kept, since a storage fault is
/// not the event's end.
///
/// The event's stored record is made unreadable for one poll, then restored.
#[tracing::instrument(level = "debug", skip_all)]
async fn unreadable_event_keeps_the_delegation(case: &Case<'_>) -> Result {
	let member = "unreadable";
	let delay_id = case.schedule(member, HOUR_MS).await?;

	case.delegated(PREFIXES[0], member, &delay_id)
		.await?;

	let events = &case.services.db["delayid_event"];
	let stored = events.get(&delay_id).await?.to_vec();

	events.insert(&delay_id, b"{");
	case.poll(now_millis()).await;
	events.insert(&delay_id, &stored);

	assert_eq!(case.stored(member).await?, (delay_id.clone(), false));

	case.cancel(&delay_id).await
}

/// Delegates an event once LiveKit has been asked, then lets LiveKit give
/// its answer.
///
/// LiveKit answers only after the new delegation is stored, so the poll waiting
/// on it must find the replacement when it reads the delegation again.
#[tracing::instrument(level = "debug", skip_all)]
async fn redelegate(
	case: &Case<'_>,
	seen: &mut UnboundedReceiver<Seen>,
	member: &str,
	delay_id: &str,
	answer: Answer,
) -> Result {
	seen.recv()
		.await
		.ok_or_else(|| err!("the stand-in closed"))?;

	case.delegated(PREFIXES[0], member, delay_id)
		.await?;

	case.answer.send_replace(answer);

	Ok(())
}

/// The stand-in LiveKit: records each request, then answers as the case set.
///
/// A held answer waits for the case to set another, and gives that one.
#[tracing::instrument(level = "debug", skip_all)]
async fn stand_in(State(stand_in): State<StandIn>, request: Request) -> AxumResponse {
	let (parts, body) = request.into_parts();
	let bytes = to_bytes(body, usize::MAX)
		.await
		.unwrap_or_default();

	let body: Value = from_slice(&bytes).unwrap_or_default();
	let identity = body["identity"].clone();
	let authorization = parts
		.headers
		.get(AUTHORIZATION)
		.and_then(|value| value.to_str().ok())
		.unwrap_or_default()
		.to_owned();

	let seen = Seen {
		path: parts.uri.path().to_owned(),
		authorization,
		body,
	};

	stand_in.seen.send(seen).ok();

	let answer = stand_in
		.answer
		.clone()
		.wait_for(|answer| *answer != Answer::Hold)
		.await
		.map_or(Answer::Failure, |answer| *answer);

	match answer {
		| Answer::State(state) =>
			Json(json!({ "identity": identity, "state": state })).into_response(),
		| Answer::Unknown => {
			let error = json!({ "code": "not_found", "msg": "participant does not exist" });

			(StatusCode::NOT_FOUND, Json(error)).into_response()
		},
		| Answer::Failure | Answer::Hold => {
			let error = json!({ "code": "internal", "msg": "stand-in failure" });

			(StatusCode::INTERNAL_SERVER_ERROR, Json(error)).into_response()
		},
	}
}

/// Takes the stand-in's next request and checks it asked about the member.
///
/// It must be a GetParticipant for the member's derived identity in the
/// call's derived room, carrying a token that may administer that room.
fn asked_about(case: &Case<'_>, seen: &mut UnboundedReceiver<Seen>, member: &str) -> Result {
	let request = seen
		.try_recv()
		.map_err(|e| err!("LiveKit was not asked: {e}"))?;

	let bearer = request
		.authorization
		.strip_prefix("Bearer ")
		.ok_or_else(|| err!("no Bearer token on {}", request.path))?;

	let room = derive(case.room_id.as_str(), FOLDED_SLOT);
	let identity = derive(case.user_id.as_str(), member);
	let grant = &claims(bearer)?["video"];

	assert_eq!(request.path, GET_PARTICIPANT);
	assert_eq!(request.body, json!({ "room": room, "identity": identity }));
	assert_eq!((&grant["room"], &grant["roomAdmin"]), (&json!(room), &json!(true)), "{grant}");

	Ok(())
}

fn drain(seen: &mut UnboundedReceiver<Seen>) { while seen.try_recv().is_ok() {} }

fn without(object: &Value, key: &str) -> Value {
	let fields: Map<String, Value> = object
		.as_object()
		.into_iter()
		.flatten()
		.filter(|(name, _)| *name != key)
		.map(|(name, value)| (name.clone(), value.clone()))
		.collect();

	fields.into()
}

#[implement(Case, params = "<'_>")]
#[tracing::instrument(level = "debug", skip(self))]
async fn schedule(&self, member: &str, delay_ms: u64) -> Result<String> {
	let room_id = self.room_id;
	let path = format!(
		"rooms/{room_id}/state/{CALL_MEMBER}/{member}?org.matrix.msc4140.delay={delay_ms}"
	);

	let response = self
		.caller
		.request(Method::PUT, &self.caller.url(&path), &json!({}))
		.await?;

	let reply = accepted(response).await?;

	Ok(field(&reply, "delay_id")?.to_owned())
}

#[implement(Case, params = "<'_>")]
async fn delegated(&self, prefix: &str, member: &str, delay_id: &str) -> Result {
	let response = self.delegate(prefix, member, delay_id).await?;
	let reply = accepted(response).await?;

	assert_eq!(reply, json!({}), "{prefix}");

	Ok(())
}

#[implement(Case, params = "<'_>")]
#[tracing::instrument(level = "debug", skip(self))]
async fn delegate(&self, prefix: &str, member: &str, delay_id: &str) -> Result<Response> {
	let url = self.caller.client_url(prefix, DELEGATE);

	self.caller
		.post_url(&url, &self.body(member, delay_id))
		.await
}

#[implement(Case, params = "<'_>")]
fn body(&self, member: &str, delay_id: &str) -> Value {
	json!({
		"url": self.url,
		"room_id": self.room_id,
		"slot_id": SLOT,
		"member_id": member,
		"delay_id": delay_id,
	})
}

/// The `delay_id` and connected flag of a member's stored delegation.
///
/// A record without the flag reads as not connected, as the server reads it.
#[implement(Case, params = "<'_>")]
async fn stored(&self, member: &str) -> Result<(String, bool)> {
	let delegation = self
		.delegation(member)
		.await
		.ok_or_else(|| err!("no delegation is stored for {member}"))?;

	let delay_id = field(&delegation, "delay_id")?.to_owned();
	let connected = delegation["connected"]
		.as_bool()
		.unwrap_or_default();

	Ok((delay_id, connected))
}

#[implement(Case, params = "<'_>")]
fn store(&self, member: &str, record: &Value) {
	let room = derive(self.room_id.as_str(), FOLDED_SLOT);
	let identity = derive(self.user_id.as_str(), member);
	let bytes = serde_json::to_vec(record).expect("a JSON value serializes");

	self.services.db["lkparticipant_delegation"]
		.put_raw((room.as_str(), identity.as_str()), bytes);
}

/// Reads the delegation stored for one of the caller's call members.
///
/// The key is derived here by MSC4195's construction, so a delegation stored
/// under any other room or identity reads as none.
#[implement(Case, params = "<'_>")]
async fn delegation(&self, member: &str) -> Option<Value> {
	let room = derive(self.room_id.as_str(), FOLDED_SLOT);
	let identity = derive(self.user_id.as_str(), member);
	let row = self.services.db["lkparticipant_delegation"]
		.qry(&(room.as_str(), identity.as_str()))
		.await
		.ok()?;

	from_slice(&row).ok()
}

#[implement(Case, params = "<'_>")]
async fn event(&self, delay_id: &str) -> Result<Event> {
	self.services
		.delayed_events
		.get(delay_id, None)
		.await
}

#[implement(Case, params = "<'_>")]
async fn cancel(&self, delay_id: &str) -> Result {
	self.services
		.delayed_events
		.cancel(delay_id, None)
		.await
}

#[implement(Case, params = "<'_>")]
async fn poll(&self, now: u64) { self.services.livekit.poll_delegations(now).await; }
