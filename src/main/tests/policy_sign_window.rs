#![cfg(test)]
#![allow(clippy::expect_used)]

use std::{
	net::TcpListener,
	path::PathBuf,
	pin::pin,
	sync::{Arc, Mutex},
	time::Duration,
};

use axum::{Json, Router, body::Bytes, extract::State, http::StatusCode, routing::post};
use axum_server::{from_tcp_rustls, tls_rustls::RustlsConfig};
use futures::{
	StreamExt, TryStreamExt,
	future::{Either, ready, select},
};
use reqwest::Method;
use serde_json::{Value, from_value, json};
use tokio::{
	sync::Notify,
	task::JoinSet,
	time::{sleep, timeout},
};
use tuwunel_core::{
	Err, Result, err, implement,
	itertools::Itertools,
	ruma::{
		CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId, OwnedRoomId,
		OwnedServerName, RoomId, RoomVersionId, ServerName, UserId,
		canonical_json::redact,
		events::{
			StateEventType,
			room::{
				member::{MembershipState, RoomMemberEventContent},
				message::RoomMessageEventContent,
				policy::RoomPolicyEventContent,
				redaction::RoomRedactionEventContent,
				topic::RoomTopicEventContent,
			},
		},
		serde::{Base64, Raw},
		signatures::{Ed25519KeyPair, KeyPair, to_canonical_json_string_for_signing},
	},
};
use tuwunel_matrix::{Event, event::gen_event_id, pdu::PduBuilder, room_version::rules};
use tuwunel_service::Services;

use self::{
	client::{Client, register},
	fixture::boot,
	state_cache::membership_update,
};

mod client;
mod fixture;
#[path = "fixture/state_cache.rs"]
mod state_cache;

const CERTIFICATE: &str = "../../nix/pkgs/complement/certificate.crt";
const PRIVATE_KEY: &str = "../../nix/pkgs/complement/private_key.key";
const KEY_ID: &str = "ed25519:policy_server";
const ALICE_TOKEN: &str = "policy-sign-window-alice-access-token";
const BOB_TOKEN: &str = "policy-sign-window-bob-access-token";
const MESSAGE: &str = "m.room.message";
const UNSTABLE_POLICY: &str = "org.matrix.msc4284.policy";

// How long a request must stay pending to count as waiting.
const WAIT: Duration = Duration::from_secs(2);

/// A room policy server whose answers the test controls.
///
/// It signs every event it is asked about, except that it parks the next request
/// about one room until the test releases it and refuses every request about
/// another. It records each event it is asked about.
struct Policy {
	name: OwnedServerName,
	key: Ed25519KeyPair,
	room_version: RoomVersionId,
	park: Mutex<Option<OwnedRoomId>>,
	refuse: Mutex<Option<OwnedRoomId>>,
	arrived: Notify,
	release: Notify,
	seen: Mutex<Vec<CanonicalJsonObject>>,
}

/// Alice, who owns each case's room, and Bob, who joins it.
///
/// Each case takes a fresh room from `room`, so cases share only the users and
/// the policy server.
struct Harness<'a> {
	services: &'a Services,
	policy: &'a Policy,
	alice: Client<'a>,
	bob: Client<'a>,
	alice_id: &'a UserId,
	bob_id: &'a UserId,
}

type Sent = (StatusCode, Value);

/// MSC4284: a client's message leaves its room unlocked while the room's policy
/// server signs it.
///
/// Others may append meanwhile; a state change rebuilds the message and signs
/// it again under the lock. Messages in one room still take turns, a retried
/// transaction waits for the first, a redaction keeps the room held, a refusal
/// stores nothing and a copy relayed back during the window is not stored
/// twice.
#[test]
fn policy_signing_leaves_the_room_unlocked() -> Result {
	let options = [
		"network.http.allow_invalid_tls_certificates=true",
		"network.http.ip_range_denylist=[]",
		"rooms.policy_server.request_timeout=60",
	];

	boot("policy-sign-window", options, exercise)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, base: &str) -> Result {
	let alice_id = register(services, "alice", ALICE_TOKEN).await?;
	let bob_id = register(services, "bob", BOB_TOKEN).await?;

	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let name = format!("127.0.0.1:{}", listener.local_addr()?.port());
	let key = Ed25519KeyPair::from_der(&Ed25519KeyPair::generate(), "policy_server".into())
		.map_err(|e| err!("policy server key: {e}"))?;

	let room_version = services
		.server
		.config
		.rooms
		.default_version
		.clone();

	let policy = Arc::new(Policy {
		name: ServerName::parse(&name).map_err(|e| err!("policy server name: {e}"))?,
		key,
		room_version,
		park: Mutex::default(),
		refuse: Mutex::default(),
		arrived: Notify::new(),
		release: Notify::new(),
		seen: Mutex::default(),
	});

	listener.set_nonblocking(true)?;

	let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
	let config =
		RustlsConfig::from_pem_file(manifest.join(CERTIFICATE), manifest.join(PRIVATE_KEY))
			.await?;

	let app = Router::new()
		.route("/_matrix/policy/v1/sign", post(sign))
		.fallback(accept)
		.with_state(policy.clone());

	let remote = from_tcp_rustls(listener, config)?.serve(app.into_make_service());
	let mut tasks = JoinSet::new(); // The set aborts the stub when the exercise exits.

	tasks.spawn_on(remote, services.server.runtime());

	let h = Harness {
		services,
		policy: &policy,
		alice: Client { services, base, token: ALICE_TOKEN },
		bob: Client { services, base, token: BOB_TOKEN },
		alice_id: &alice_id,
		bob_id: &bob_id,
	};

	others_append_during_the_window(&h, &h.room(1).await?).await?;
	a_state_change_rebuilds_the_message(&h, &h.room(2).await?).await?;
	a_policy_change_rebuilds_the_message(&h, &h.room(9).await?).await?;
	a_sender_removed_meanwhile_is_refused(&h, &h.room(3).await?).await?;
	a_retried_transaction_waits_for_the_first(&h, &h.room(4).await?).await?;
	messages_keep_their_arrival_order(&h, &h.room(5).await?).await?;
	a_redaction_keeps_the_room_held(&h, &h.room(6).await?).await?;
	a_refusal_stores_nothing(&h, &h.room(7).await?).await?;
	a_relayed_copy_is_stored_once(&h, &h.room(8).await?).await
}

/// Bob's message lands while Alice's waits for its signature.
///
/// The state lock is free in that window, and both events stay forward
/// extremities until the next message cites them.
async fn others_append_during_the_window(h: &Harness<'_>, room_id: &RoomId) -> Result {
	let content = text("one");
	let sent = h.park_and_send(&h.alice, room_id, MESSAGE, "t1", &content);
	let sent = pin!(sent);
	let sent = parked(h.policy, sent).await?;
	let meanwhile = timeout(WAIT, h.append(room_id, h.bob_id, message("meanwhile"))).await;

	h.policy.release.notify_one();

	let event_id = sent_event(sent.await?)?;
	let meanwhile = meanwhile.map_err(|_| err!("the room stayed locked in the window"))??;
	let leaves = sorted([meanwhile, event_id]);

	assert_eq!(h.extremities(room_id).await, leaves);

	let next = send(&h.alice, room_id, MESSAGE, "t1b", &text("two")).await?;
	let next = sent_event(next)?;

	assert_eq!(h.prev_events(&next).await?, leaves, "the next message cites both leaves");
	assert_eq!(h.extremities(room_id).await, [next]);

	Ok(())
}

/// A state change while the message waits discards it.
///
/// The message is built again on the new state and signed again under the
/// lock, and the first build is never stored.
async fn a_state_change_rebuilds_the_message(h: &Harness<'_>, room_id: &RoomId) -> Result {
	let content = text("rebuilt");
	let sent = h.park_and_send(&h.bob, room_id, MESSAGE, "t2", &content);
	let sent = pin!(sent);
	let sent = parked(h.policy, sent).await?;
	let topic = PduBuilder::state("", &RoomTopicEventContent::new("moved".into()));
	let topic = timeout(WAIT, h.append(room_id, h.alice_id, topic)).await;

	h.policy.release.notify_one();

	let event_id = sent_event(sent.await?)?;
	let topic = topic.map_err(|_| err!("the room stayed locked in the window"))??;
	let asked = h.asked(room_id, h.bob_id);
	let first = h.event_id(&asked[0])?;
	let state = &h.services.state;

	assert_eq!(asked.len(), 2, "the rebuilt message is signed again");
	assert!(h.prev_events(&event_id).await?.contains(&topic));
	assert_ne!(first, event_id);
	let stored = h.services.timeline.get_pdu_id(&first).await;

	assert!(stored.is_err(), "the first build was stored");
	assert_eq!(
		state.pdu_shortstatehash(&event_id).await?,
		state.get_room_shortstatehash(room_id).await?,
	);

	Ok(())
}

/// A change of policy server while the message waits is honoured.
///
/// Redacting the room's policy event leaves room state as it was and brings back
/// an older policy naming another key, so the message is built again and fails
/// verification under that key rather than going out.
async fn a_policy_change_rebuilds_the_message(h: &Harness<'_>, room_id: &RoomId) -> Result {
	let other = Ed25519KeyPair::from_der(&Ed25519KeyPair::generate(), "other".into())
		.map_err(|e| err!("other policy key: {e}"))?;

	let public_key: Base64 = Base64::new(other.public_key().to_vec());
	let older = json!({ "via": h.policy.name, "public_key": public_key });
	let older = PduBuilder {
		event_type: UNSTABLE_POLICY.into(),
		content: Raw::new(&from_value(older)?)?,
		state_key: Some("".into()),
		..PduBuilder::default()
	};

	h.append(room_id, h.alice_id, older).await?;

	let policy = h
		.services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomPolicy, "")
		.await?;

	let content = text("stale");
	let sent = h.park_and_send(&h.bob, room_id, MESSAGE, "t9", &content);
	let sent = pin!(sent);
	let sent = parked(h.policy, sent).await?;
	let redaction = RoomRedactionEventContent::new_v11(policy.event_id.clone());
	let redaction = PduBuilder {
		redacts: Some(policy.event_id),
		..PduBuilder::timeline(&redaction)
	};

	let redacted = timeout(WAIT, h.append(room_id, h.alice_id, redaction)).await;

	h.policy.release.notify_one();

	let (status, body) = sent.await?;

	redacted.map_err(|_| err!("the room stayed locked in the window"))??;

	assert_eq!(
		status,
		StatusCode::SERVICE_UNAVAILABLE,
		"sent under the redacted policy: {body}"
	);

	assert_eq!(h.asked(room_id, h.bob_id).len(), 2, "the message was not built again");

	Ok(())
}

/// Bob, kicked while his message waits, is refused.
///
/// The kick changes room state, so the message is built again, and the auth
/// check on the new state refuses it.
async fn a_sender_removed_meanwhile_is_refused(h: &Harness<'_>, room_id: &RoomId) -> Result {
	let content = text("too late");
	let sent = h.park_and_send(&h.bob, room_id, MESSAGE, "t3", &content);
	let sent = pin!(sent);
	let sent = parked(h.policy, sent).await?;
	let kick = RoomMemberEventContent::new(MembershipState::Leave);
	let kick = PduBuilder::state(h.bob_id.as_str(), &kick);
	let kick = timeout(WAIT, h.append(room_id, h.alice_id, kick)).await;

	h.policy.release.notify_one();

	let (status, body) = sent.await?;
	let kick = kick.map_err(|_| err!("the room stayed locked in the window"))??;

	assert_eq!(status, StatusCode::FORBIDDEN);
	assert_eq!(body["errcode"], "M_FORBIDDEN");
	assert_eq!(h.latest(room_id).await?, kick);

	Ok(())
}

/// A retried transaction waits for the request it repeats.
///
/// Both answer the same event, which the policy server was asked about once.
async fn a_retried_transaction_waits_for_the_first(h: &Harness<'_>, room_id: &RoomId) -> Result {
	let content = text("once");
	let sent = h.park_and_send(&h.alice, room_id, MESSAGE, "t4", &content);
	let sent = pin!(sent);
	let sent = parked(h.policy, sent).await?;
	let retry = send(&h.alice, room_id, MESSAGE, "t4", &content);
	let retry = pin!(retry);
	let retry = waiting(retry).await;

	h.policy.release.notify_one();

	let event_id = sent_event(sent.await?)?;
	let retry = retry.ok_or_else(|| err!("the retry did not wait for the first request"))?;

	assert_eq!(sent_event(retry.await?)?, event_id);
	assert_eq!(h.asked(room_id, h.alice_id).len(), 1, "the retry was signed too");

	Ok(())
}

/// Bob's message waits for Alice's, which arrived first.
///
/// The room's messages take turns, so Bob's is built after Alice's is stored
/// and cites it.
async fn messages_keep_their_arrival_order(h: &Harness<'_>, room_id: &RoomId) -> Result {
	let content = text("first");
	let sent = h.park_and_send(&h.alice, room_id, MESSAGE, "t5", &content);
	let sent = pin!(sent);
	let sent = parked(h.policy, sent).await?;
	let later_content = text("second");
	let later = send(&h.bob, room_id, MESSAGE, "t5", &later_content);
	let later = pin!(later);
	let later = waiting(later).await;

	h.policy.release.notify_one();

	let first = sent_event(sent.await?)?;
	let later = later.ok_or_else(|| err!("Bob's message did not wait for Alice's"))?;
	let second = sent_event(later.await?)?;

	assert_eq!(h.prev_events(&second).await?, [first]);
	assert_eq!(h.latest(room_id).await?, second);

	Ok(())
}

/// A redaction sent as a message holds the room while it is signed.
///
/// Whether its sender may redact depends on the redacted event, so it keeps
/// the continuous hold.
async fn a_redaction_keeps_the_room_held(h: &Harness<'_>, room_id: &RoomId) -> Result {
	let target = send(&h.alice, room_id, MESSAGE, "t6", &text("gone")).await?;
	let redaction = json!({ "redacts": sent_event(target)? });
	let sent = h.park_and_send(&h.alice, room_id, "m.room.redaction", "t6b", &redaction);
	let sent = pin!(sent);
	let sent = parked(h.policy, sent).await?;
	let unlocked = timeout(WAIT, h.services.state.mutex.lock(room_id))
		.await
		.is_ok();

	h.policy.release.notify_one();
	sent_event(sent.await?)?;

	assert!(!unlocked, "the room was unlocked while the redaction was signed");

	Ok(())
}

/// A refused message stores nothing and records no transaction.
///
/// The policy server's error reaches the client, and a retry of the same
/// transaction asks the policy server again.
async fn a_refusal_stores_nothing(h: &Harness<'_>, room_id: &RoomId) -> Result {
	let before = (h.extremities(room_id).await, h.latest(room_id).await?);

	h.policy.refuse(room_id);

	let (status, body) = send(&h.alice, room_id, MESSAGE, "t7", &text("no")).await?;
	let after = (h.extremities(room_id).await, h.latest(room_id).await?);

	assert_eq!(status, StatusCode::BAD_REQUEST, "the policy server's answer is relayed");
	assert_eq!(body["errcode"], "M_FORBIDDEN");
	assert_eq!(after, before);

	send(&h.alice, room_id, MESSAGE, "t7", &text("no")).await?;

	assert_eq!(h.asked(room_id, h.alice_id).len(), 2, "the retry was answered from a record");

	Ok(())
}

/// A copy of the waiting message relayed back meanwhile is stored only once.
///
/// The receive path stores the copy while the room is unlocked, and the send
/// answers it rather than storing the message again.
async fn a_relayed_copy_is_stored_once(h: &Harness<'_>, room_id: &RoomId) -> Result {
	let content = text("relayed");
	let sent = h.park_and_send(&h.alice, room_id, MESSAGE, "t8", &content);
	let sent = pin!(sent);
	let sent = parked(h.policy, sent).await?;
	let relayed = h.asked(room_id, h.alice_id).remove(0);
	let event_id = h.event_id(&relayed)?;
	let ingest = h.services.event_handler.handle_incoming_pdu(
		&h.policy.name,
		room_id,
		&event_id,
		relayed,
		true,
	);

	let ingested = timeout(WAIT, ingest).await;

	h.policy.release.notify_one();

	let answered = sent_event(sent.await?)?;

	ingested.map_err(|_| err!("the room stayed locked in the window"))??;

	let rows = h
		.services
		.timeline
		.pdus(None, room_id, None)
		.try_filter(|(_, pdu)| ready(pdu.event_id == event_id))
		.count()
		.await;

	assert_eq!(answered, event_id);
	assert_eq!(rows, 1, "the relayed message has more than one timeline row");

	Ok(())
}

/// A public room of Alice's that Bob joined, whose policy server is the stub.
///
/// The stub's server is marked joined without a membership event.
#[implement(Harness, params = "<'_>")]
async fn room(&self, n: usize) -> Result<OwnedRoomId> {
	let room_id = self
		.alice
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	self.bob
		.post(&format!("rooms/{room_id}/join"), &json!({}))
		.await?;

	let member = UserId::parse(format!("@policy{n}:{}", self.policy.name))?;
	let count = *self.services.globals.next_count();

	self.services
		.state_cache
		.update_membership(membership_update(&room_id, &member, MembershipState::Join, count))
		.await?;

	let public_key = Base64::new(self.policy.key.public_key().to_vec());
	let content = RoomPolicyEventContent::new(self.policy.name.clone(), public_key);

	self.append(&room_id, self.alice_id, PduBuilder::state("", &content))
		.await?;

	Ok(room_id)
}

#[implement(Harness, params = "<'_>")]
async fn park_and_send(
	&self,
	client: &Client<'_>,
	room_id: &RoomId,
	event_type: &str,
	txn_id: &str,
	content: &Value,
) -> Result<Sent> {
	self.policy.park(room_id);

	send(client, room_id, event_type, txn_id, content).await
}

#[implement(Harness, params = "<'_>")]
async fn append(
	&self,
	room_id: &RoomId,
	sender: &UserId,
	builder: PduBuilder,
) -> Result<OwnedEventId> {
	let state_lock = self.services.state.mutex.lock(room_id).await;

	self.services
		.timeline
		.build_and_append_pdu(builder, sender, room_id, &state_lock)
		.await
}

#[implement(Harness, params = "<'_>")]
async fn extremities(&self, room_id: &RoomId) -> Vec<OwnedEventId> {
	let extremities: Vec<_> = self
		.services
		.state
		.get_forward_extremities(room_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	sorted(extremities)
}

#[implement(Harness, params = "<'_>")]
async fn prev_events(&self, event_id: &EventId) -> Result<Vec<OwnedEventId>> {
	let pdu = self.services.timeline.get_pdu(event_id).await?;
	let prev_events = sorted(pdu.prev_events().map(ToOwned::to_owned));

	Ok(prev_events)
}

#[implement(Harness, params = "<'_>")]
async fn latest(&self, room_id: &RoomId) -> Result<OwnedEventId> {
	let pdu = self
		.services
		.timeline
		.latest_pdu_in_room(room_id)
		.await?;

	Ok(pdu.event_id)
}

/// The events the policy server was asked about in a room, from one sender.
///
/// They come in arrival order, so the first is the sender's first build.
#[implement(Harness, params = "<'_>")]
fn asked(&self, room_id: &RoomId, sender: &UserId) -> Vec<CanonicalJsonObject> {
	self.policy
		.seen
		.lock()
		.expect("seen events")
		.iter()
		.filter(|event| field(event, "room_id").eq(&Some(room_id.as_str())))
		.filter(|event| field(event, "sender").eq(&Some(sender.as_str())))
		.cloned()
		.collect()
}

#[implement(Harness, params = "<'_>")]
fn event_id(&self, event: &CanonicalJsonObject) -> Result<OwnedEventId> {
	gen_event_id(event, &self.policy.room_version)
}

#[implement(Policy)]
fn park(&self, room_id: &RoomId) {
	*self.park.lock().expect("parked room") = Some(room_id.to_owned());
}

#[implement(Policy)]
fn refuse(&self, room_id: &RoomId) {
	*self.refuse.lock().expect("refused room") = Some(room_id.to_owned());
}

/// Signs the event, unless its room is the one to park or to refuse.
///
/// A parked request waits for the test's release before it is answered.
async fn sign(State(policy): State<Arc<Policy>>, body: Bytes) -> (StatusCode, Json<Value>) {
	let event: CanonicalJsonObject =
		serde_json::from_slice(&body).expect("the request body is an event");

	let room_id = field(&event, "room_id").map(ToOwned::to_owned);
	let in_room = |room: &OwnedRoomId| Some(room.as_str()) == room_id.as_deref();

	policy
		.seen
		.lock()
		.expect("seen events")
		.push(event.clone());

	let parked = policy
		.park
		.lock()
		.expect("parked room")
		.take_if(|room| in_room(room))
		.is_some();

	if parked {
		policy.arrived.notify_one();
		policy.release.notified().await;
	}

	let refused = policy
		.refuse
		.lock()
		.expect("refused room")
		.as_ref()
		.is_some_and(in_room);

	if refused {
		let error = json!({ "errcode": "M_FORBIDDEN", "error": "Refused by the policy." });

		return (StatusCode::BAD_REQUEST, Json(error));
	}

	let rules = rules(&policy.room_version).expect("a supported room version");
	let redacted = redact(event, &rules.redaction, None).expect("a redactable event");
	let canonical = to_canonical_json_string_for_signing(&redacted).expect("a canonical event");
	let signature = policy.key.sign(canonical.as_bytes()).base64();
	let name = policy.name.as_str();

	(StatusCode::OK, Json(json!({ name: { KEY_ID: signature } })))
}

/// Accepts every other federation request.
///
/// A failed delivery would put the policy server in backoff, and this server
/// would then stop asking it to sign.
async fn accept() -> Json<Value> { Json(json!({ "pdus": {} })) }

/// Polls a request until the policy server parks the signing request it makes.
///
/// The request is handed back still pending, for the case to finish once it
/// releases the policy server.
async fn parked<F>(policy: &Policy, request: F) -> Result<F>
where
	F: Future + Unpin,
{
	match select(request, pin!(policy.arrived.notified())).await {
		| Either::Left(_) => Err!("the request finished before its policy server answered"),
		| Either::Right(((), request)) => Ok(request),
	}
}

/// Polls a request for a while.
///
/// It is handed back when still pending, as a request waiting on a lock is.
async fn waiting<F>(request: F) -> Option<F>
where
	F: Future + Unpin,
{
	match select(request, pin!(sleep(WAIT))).await {
		| Either::Left(_) => None,
		| Either::Right(((), request)) => Some(request),
	}
}

async fn send(
	client: &Client<'_>,
	room_id: &RoomId,
	event_type: &str,
	txn_id: &str,
	content: &Value,
) -> Result<Sent> {
	let url = client.url(&format!("rooms/{room_id}/send/{event_type}/{txn_id}"));
	let response = client.request(Method::PUT, &url, content).await?;
	let status = response.status();

	Ok((status, response.json().await?))
}

fn sent_event((status, body): Sent) -> Result<OwnedEventId> {
	if status != StatusCode::OK {
		return Err!("the send failed with {status}: {body}");
	}

	body["event_id"]
		.as_str()
		.ok_or_else(|| err!("the send answered no event_id: {body}"))?
		.try_into()
		.map_err(|e| err!("the send answered an invalid event_id: {e}"))
}

fn text(body: &str) -> Value { json!({ "msgtype": "m.text", "body": body }) }

fn message(body: &str) -> PduBuilder {
	PduBuilder::timeline(&RoomMessageEventContent::text_plain(body))
}

fn field<'a>(event: &'a CanonicalJsonObject, name: &str) -> Option<&'a str> {
	event
		.get(name)
		.and_then(CanonicalJsonValue::as_str)
}

fn sorted<I>(event_ids: I) -> Vec<OwnedEventId>
where
	I: IntoIterator<Item = OwnedEventId>,
{
	event_ids.into_iter().sorted_unstable().collect()
}
