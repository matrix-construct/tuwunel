#![cfg(test)]

use std::{net::TcpListener, pin::pin, sync::atomic::Ordering, time::Duration};

use axum::http::StatusCode;
use futures::{
	TryStreamExt,
	future::{Either, ready, select, try_join},
};
use reqwest::{Method, Response};
use serde_json::{Value, json, to_value};
use tokio::time::sleep;
use tuwunel_core::{
	Result, err,
	result::NotFound,
	ruma::{
		EventId, RoomId, ServerName, ServerSigningKeyId, UserId,
		events::{
			StateEventType,
			room::{
				member::{MembershipState, RoomMemberEventContent},
				policy::RoomPolicyEventContent,
			},
		},
		serde::Base64,
	},
};
use tuwunel_matrix::{
	Event,
	pdu::{PduBuilder, PduEvent},
};
use tuwunel_service::{Services, membership::RemoteInvite};

use self::{
	appservice::{Bridge, register_appservice},
	client::{Client, accepted, field, register},
	fixture::boot,
	state_cache::membership_update,
	stub::{Peer, serve},
};

mod appservice;
mod client;
mod fixture;

#[path = "client/get.rs"]
mod get;

#[path = "fixture/server_keys.rs"]
mod server_keys;

#[path = "fixture/state_cache.rs"]
mod state_cache;

#[path = "state_member_invite/stub.rs"]
mod stub;

const KEY_ID: &str = "ed25519:stub";
const UNPUBLISHED_KEY_ID: &str = "ed25519:unpublished";
const POLICY_KEY_ID: &str = "ed25519:policy_server";
const DECOY: &str = "decoy.example";
const UNLISTED: &str = "unlisted.example";
const REFUSED: &str = "@refused:";
const UNSIGNED: &str = "@unsigned:";
const FORGED: &str = "@forged:";
const STALLED: &str = "@stalled:";
const RETRIED: &str = "@retried:";
const POLICY: &str = "@policy:";
const NO_ANSWER: &str = "did not answer the invite";
const ALICE_TOKEN: &str = "state-member-invite-alice-access-token";
const BOB_TOKEN: &str = "state-member-invite-bob-access-token";
const BRIDGE_TOKEN: &str = "state-member-invite-bridge-access-token";
const TIMESTAMP: u64 = 1_234_567_890_123;
const QUIET: [&str; 2] = ["logging.enable=false", "database.pool.max_workers=64"];
const DELAY_PREFIX: &str = "unstable/org.matrix.msc4140/";
const DELAY_PARAM: &str = "org.matrix.msc4140.delay";
const DELAY_ID: &str = "org.matrix.msc4140.delay_id";

// Long enough that no delayed invite goes out before its case sends it.
const LONG_DELAY: u64 = 60_000;

const ANSWER_WITHIN: Duration = Duration::from_secs(1);

// An invite nothing holds back is stored well within this, so one still
// pending at its end is waiting on the mutex.
const PARKED: Duration = Duration::from_secs(2);

// The stub's certificate and loopback address pass, and its key comes from
// itself rather than a notary.
const STUB_OPTIONS: [&str; 4] = [
	"network.http.allow_invalid_tls_certificates=true",
	"network.http.ip_range_denylist=[]",
	"federation.keys.trusted_servers=[]",
	// Outlasts `STALL`, so the stalled cases meet the invite bounds.
	"federation.transport.timeout=60",
];

/// Runs each case on a server of its own, booted in turn.
///
/// The second case takes keys only from the stub as its notary. The last
/// blocks invites from non-admins server-wide, which would refuse the first
/// case's senders before any invite left the server.
#[test]
fn member_invites_sent_as_state() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let notary_listener = TcpListener::bind(("127.0.0.1", 0))?;
	let notary = notary_listener.local_addr()?;
	let trusted_servers = format!("federation.keys.trusted_servers=[\"{notary}\"]");
	let federating = QUIET.into_iter().chain(STUB_OPTIONS);
	let notarized = federating
		.clone()
		.chain([trusted_servers.as_str(), "federation.keys.only_query_trusted_servers=true"]);

	let blocking = QUIET
		.into_iter()
		.chain(["rooms.invites.block_non_admin=true"]);

	boot("state-member-invite-remote", federating, async move |services, base| {
		remote_invites(services, base, listener).await
	})?;

	boot("state-member-invite-notary", notarized, async move |services, base| {
		notarized_invite(services, base, notary_listener).await
	})?;

	boot("state-member-invite-blocked", blocking, non_admin_invite_refused)
}

/// Member invites sent as state to users on the stub reach it as invites.
///
/// The state route asks the invitee's server before storing such an invite,
/// since that server drops a copy it gets in a transaction while it is not in
/// the room. An accepted invite is stored as this server built it with the
/// stub's signature added, and a refusal reaches the client with nothing
/// stored. A delayed invite takes the same path when sent. The stalled cases
/// run last, since the stub logs their invites only once it answers.
async fn remote_invites(services: &Services, base: &str, listener: TcpListener) -> Result {
	let (peer, _stub) = serve(services, listener).await?;
	let alice = register(services, "alice", ALICE_TOKEN).await?;
	let client = Client { services, base, token: ALICE_TOKEN };

	unverified_invite(&client, &peer, UNSIGNED, "lacks the invitee server's signature").await?;
	unverified_invite(&client, &peer, FORGED, "countersignature failed verification").await?;
	ordinary_invite(&client, &alice, &peer).await?;
	policy_server_invite(&client, &alice, &peer).await?;
	appservice_invite(services, base, &peer).await?;
	disabled_room_invite(&client, &peer).await?;
	parked_invite(&client, &peer).await?;
	delayed_invite(&client, &alice, &peer).await?;
	delayed_refusal(&client, &peer).await?;
	stalled_invite(&client, &alice, &peer).await?;
	delayed_stalled_invite(&client, &peer).await
}

/// A notary's answer yields a key only from a document naming the server
/// asked about.
///
/// The stub is this server's only notary and lists the decoy's document, under
/// the stub's key ID, before its own. The stub's invite still verifies, a
/// server neither document names gets no key, and the decoy's document is
/// never cached.
async fn notarized_invite(services: &Services, base: &str, listener: TcpListener) -> Result {
	let (peer, _stub) = serve(services, listener).await?;
	let alice = register(services, "alice", ALICE_TOKEN).await?;
	let client = Client { services, base, token: ALICE_TOKEN };
	let key_id = <&ServerSigningKeyId>::try_from(KEY_ID)?;
	let unlisted = <&ServerName>::try_from(UNLISTED)?;
	let decoy = <&ServerName>::try_from(DECOY)?;

	ordinary_invite(&client, &alice, &peer).await?;

	let unlisted_key = services
		.server_keys
		.get_verify_key(unlisted, key_id)
		.await;

	let decoy_cached = services
		.server_keys
		.verify_key_exists(decoy, key_id)
		.await;

	assert!(unlisted_key.is_err(), "{UNLISTED} got the decoy's key");
	assert!(!decoy_cached, "the decoy's document was cached");

	Ok(())
}

/// An invite the invitee's server answers without its valid signature stores
/// nothing.
///
/// The invitee's localpart selects the answer: the `unsigned` user's carries
/// the event on this server's signature alone, and the `forged` user's a
/// signature under a key the stub does not publish. Either way the client gets
/// a server error naming the reason instead.
async fn unverified_invite(
	client: &Client<'_>,
	peer: &Peer,
	localpart: &str,
	reason: &str,
) -> Result {
	let room_id = client.create_room(&json!({})).await?;
	let user_id = format!("{localpart}{}", peer.name);
	let body = json!({ "membership": "invite" });
	let response = put_invite(client, &room_id, &user_id, "", &body).await?;
	let error = answered(response, StatusCode::INTERNAL_SERVER_ERROR).await?;

	assert_eq!(error["errcode"], "M_UNKNOWN", "{error}");
	assert!(
		error["error"]
			.as_str()
			.is_some_and(|message| message.contains(reason)),
		"{error}"
	);

	assert!(absent(client.services, &room_id, &user_id).await, "the invite was stored");

	received(peer).map(drop)
}

/// A user's invite sent as state reaches the invitee's server.
///
/// The invite it saw keeps the client's own content keys and carries the
/// room's create event as invite state. The stored invite carries the stub's
/// signature, but not the one under a key the stub does not publish. A refused
/// invite stores nothing.
async fn ordinary_invite(client: &Client<'_>, alice: &UserId, peer: &Peer) -> Result {
	let services = client.services;
	let room_id = client.create_room(&json!({})).await?;
	let bob = format!("@bob:{}", peer.name);
	let body = json!({ "membership": "invite", "fi.mau.will_auto_accept": true });
	let response = put_invite(client, &room_id, &bob, "", &body).await?;
	let stored = invited(response, services, &room_id, &bob).await?;
	let invite = received(peer)?;
	let stored_json = services
		.timeline
		.get_pdu_json(stored.event_id())
		.await?;

	let stored_json = to_value(stored_json)?;

	assert_eq!(stored.get_content_as_value()["fi.mau.will_auto_accept"], true);
	assert!(
		stored_json["signatures"][peer.name.as_str()][KEY_ID].is_string(),
		"{stored_json}"
	);

	assert!(
		stored_json["signatures"][peer.name.as_str()][UNPUBLISHED_KEY_ID].is_null(),
		"{stored_json}"
	);

	assert_eq!(invite["event"]["sender"], alice.as_str());
	assert_eq!(invite["event"]["state_key"], bob.as_str());
	assert_eq!(invite["event"]["content"]["fi.mau.will_auto_accept"], true);
	assert!(
		invite["invite_room_state"]
			.as_array()
			.into_iter()
			.flatten()
			.any(|event| event["type"] == "m.room.create")
	);

	let refused_user = format!("{REFUSED}{}", peer.name);
	let response = put_invite(client, &room_id, &refused_user, "", &body).await?;

	refused(response, services, &room_id, &refused_user).await?;
	received(peer).map(drop)
}

/// An invite to the room's policy server keeps the policy signature it was
/// sent with, and fetches no key for it.
///
/// The stub signs the invite as policy server before it gets it as invitee,
/// then countersigns it and echoes that signature back. The stored invite
/// carries both, and with the stub's published key already cached its key
/// document is not fetched again.
async fn policy_server_invite(client: &Client<'_>, alice: &UserId, peer: &Peer) -> Result {
	let services = client.services;
	let room_id = client.create_room(&json!({})).await?;
	let member = UserId::parse(format!("@member:{}", peer.name))?;
	let count = services.globals.next_count();
	let joined = membership_update(&room_id, &member, MembershipState::Join, *count);

	services
		.state_cache
		.update_membership(joined)
		.await?;

	drop(count);

	let public_key = Base64::new(peer.policy.public_key().to_vec());
	let content = RoomPolicyEventContent::new(peer.name.clone(), public_key);
	let state_lock = services.state.mutex.lock(&room_id).await;

	services
		.timeline
		.build_and_append_pdu(PduBuilder::state("", &content), alice, &room_id, &state_lock)
		.await?;

	drop(state_lock);

	let key_id = <&ServerSigningKeyId>::try_from(KEY_ID)?;

	services
		.server_keys
		.get_verify_key(&peer.name, key_id)
		.await?;

	let fetches = peer.key_fetches.load(Ordering::Relaxed);
	let invitee = format!("{POLICY}{}", peer.name);
	let body = json!({ "membership": "invite" });
	let response = put_invite(client, &room_id, &invitee, "", &body).await?;
	let stored = invited(response, services, &room_id, &invitee).await?;
	let stored_json = services
		.timeline
		.get_pdu_json(stored.event_id())
		.await?;

	let stored_json = to_value(stored_json)?;
	let signatures = &stored_json["signatures"][peer.name.as_str()];

	assert!(signatures[POLICY_KEY_ID].is_string(), "{stored_json}");
	assert!(signatures[KEY_ID].is_string(), "{stored_json}");
	assert_eq!(peer.key_fetches.load(Ordering::Relaxed), fetches, "a key document was fetched");

	received(peer).map(drop)
}

/// An appservice's invite sent as state reaches the invitee's server.
///
/// The invite it saw names the user the appservice acted as and carries the
/// timestamp the appservice asked for. The retried-invite case reuses the
/// appservice and its room.
async fn appservice_invite(services: &Services, base: &str, peer: &Peer) -> Result {
	let bridge = Bridge {
		id: "state-member-invite",
		token: BRIDGE_TOKEN,
		sender_localpart: "bridge_bot",
		users: "^@bridged_.*$",
		aliases: None,
	};

	register(services, "bridge_bot", "state-member-invite-bridge-bot-token").await?;
	register_appservice(services, &bridge).await?;

	let bridged =
		register(services, "bridged_alice", "state-member-invite-bridged-token").await?;

	let client = Client { services, base, token: BRIDGE_TOKEN };
	let created = client
		.post_json(&format!("{}?user_id={bridged}", client.url("createRoom")), &json!({}))
		.await?;

	let room_id: &RoomId = created["room_id"]
		.as_str()
		.unwrap_or_default()
		.try_into()?;

	let carol = format!("@carol:{}", peer.name);
	let query = format!("?user_id={bridged}&ts={TIMESTAMP}");
	let body = json!({ "membership": "invite" });
	let response = put_invite(&client, room_id, &carol, &query, &body).await?;

	invited(response, services, room_id, &carol).await?;

	let invite = received(peer)?;

	assert_eq!(invite["event"]["sender"], bridged.as_str());
	assert_eq!(invite["event"]["origin_server_ts"], TIMESTAMP);

	retried_invite(&client, room_id, &bridged, peer).await
}

/// An appservice's retry of a timestamped invite, sent while the first still
/// waits on the invitee's server, stores the invite once.
///
/// Both sends build the same event from one frontier and one timestamp, and
/// the stub answers them together. The second acceptance finds the first's
/// timeline row, so it writes neither an outlier copy, which a redaction would
/// miss, nor a second timeline entry.
async fn retried_invite(
	client: &Client<'_>,
	room_id: &RoomId,
	bridged: &UserId,
	peer: &Peer,
) -> Result {
	let timeline = &client.services.timeline;
	let dana = format!("{RETRIED}{}", peer.name);
	let query = format!("?user_id={bridged}&ts={TIMESTAMP}");
	let body = json!({ "membership": "invite" });
	let put = || put_invite(client, room_id, &dana, &query, &body);
	let (first, retry) = try_join(put(), put()).await?;

	let first = answered(first, StatusCode::OK).await?;
	let retry = answered(retry, StatusCode::OK).await?;
	let event_id: &EventId = first["event_id"]
		.as_str()
		.unwrap_or_default()
		.try_into()?;

	let outlier = timeline
		.outlier_pdu_exists(event_id)
		.await
		.present()?;

	let copies = timeline
		.pdus(None, room_id, None)
		.try_filter(|(_, pdu)| ready(pdu.event_id() == event_id))
		.try_fold(0_usize, |copies, _| ready(Ok(copies.saturating_add(1))))
		.await?;

	received(peer)?;
	received(peer)?;

	assert_eq!(retry["event_id"], first["event_id"], "both sends built one event");
	assert_eq!(outlier, None, "an outlier copy was left beside the accepted invite");
	assert_eq!(copies, 1, "the invite was appended {copies} times");

	Ok(())
}

/// An invite in a room whose federation this server disabled is stored.
///
/// The invitee's server accepts it. Its countersigned copy enters as an event
/// this server built, which skips the gate that refuses a peer's event in such
/// a room, so the client gets no 403.
async fn disabled_room_invite(client: &Client<'_>, peer: &Peer) -> Result {
	let room_id = client.create_room(&json!({})).await?;
	let erin = format!("@erin:{}", peer.name);
	let body = json!({ "membership": "invite" });

	client.services.metadata.disable_room(&room_id);

	let response = put_invite(client, &room_id, &erin, "", &body).await?;

	invited(response, client.services, &room_id, &erin).await?;
	received(peer).map(drop)
}

/// An invite whose room's federation mutex is held waits for it to store.
///
/// The invitee's server answers at once, so only the mutex, which serializes a
/// retried send's acceptance, keeps the send pending. Once it is released the
/// invite is stored.
async fn parked_invite(client: &Client<'_>, peer: &Peer) -> Result {
	let room_id = client.create_room(&json!({})).await?;
	let frank = format!("@frank:{}", peer.name);
	let body = json!({ "membership": "invite" });
	let held = client
		.services
		.event_handler
		.mutex_federation
		.lock(&room_id)
		.await;

	let invite = pin!(put_invite(client, &room_id, &frank, "", &body));
	let Either::Right(((), invite)) = select(invite, pin!(sleep(PARKED))).await else {
		panic!("the invite was stored while its room's federation mutex was held");
	};

	drop(held);

	invited(invite.await?, client.services, &room_id, &frank).await?;
	received(peer).map(drop)
}

/// A delayed member invite to a user on the stub reaches it when sent.
///
/// The delayed send goes through the invite handshake, since an invite only
/// stored locally never reaches the invitee's server. The stored event still
/// names its delay ID to its sender.
async fn delayed_invite(client: &Client<'_>, alice: &UserId, peer: &Peer) -> Result {
	let room_id = client.create_room(&json!({})).await?;
	let dave = format!("@dave:{}", peer.name);
	let delay_id = schedule_invite(client, &room_id, &dave).await?;
	let response = send_delayed(client, &delay_id).await?;

	accepted(response).await?;

	let invite = received(peer)?;
	let stored = client
		.services
		.state_accessor
		.room_state_get(&room_id, &StateEventType::RoomMember, &dave)
		.await?;

	let event_id = stored.event_id();
	let event = client
		.get(&format!("rooms/{room_id}/event/{event_id}"))
		.await?;

	let delayed = delayed(client, &delay_id).await?;

	assert_eq!(invite["event"]["sender"], alice.as_str());
	assert_eq!(invite["event"]["state_key"], dave.as_str());
	assert_eq!(stored.get_content_as_value()["membership"], "invite");
	assert_eq!(delayed["finalised"]["event_id"], event_id.as_str(), "{delayed}");
	assert_eq!(event["unsigned"][DELAY_ID], delay_id.as_str(), "{event}");

	Ok(())
}

/// A delayed member invite the invitee's server refuses ends as that refusal.
///
/// The send answers with the stub's error, nothing is stored, and the
/// finalised record keeps the stub's error code.
async fn delayed_refusal(client: &Client<'_>, peer: &Peer) -> Result {
	let room_id = client.create_room(&json!({})).await?;
	let refused_user = format!("{REFUSED}{}", peer.name);
	let delay_id = schedule_invite(client, &room_id, &refused_user).await?;
	let response = send_delayed(client, &delay_id).await?;

	refused(response, client.services, &room_id, &refused_user).await?;
	received(peer)?;

	let delayed = delayed(client, &delay_id).await?;
	let finalised = &delayed["finalised"];

	assert_eq!(finalised["error"]["errcode"], "M_FORBIDDEN", "{delayed}");
	assert_eq!(finalised.get("event_id"), None, "{delayed}");

	Ok(())
}

/// An invite the invitee's server does not answer in time stores nothing.
///
/// A delayed send sets such a bound, so a stalled server cannot hold a send
/// slot. The case sets a bound shorter than the stub's stall and expects the
/// error saying the server did not answer.
async fn stalled_invite(client: &Client<'_>, alice: &UserId, peer: &Peer) -> Result {
	let services = client.services;
	let room_id = client.create_room(&json!({})).await?;
	let user_id = UserId::parse(format!("{STALLED}{}", peer.name))?;
	let content = RoomMemberEventContent::new(MembershipState::Invite);
	let state_lock = services.state.mutex.lock(&room_id).await;
	let federated = services
		.membership
		.federate_invite(RemoteInvite {
			sender: alice,
			invitee: &user_id,
			room_id: &room_id,
			invite: PduBuilder::state(user_id.as_str(), &content),
			state_lock,
			answer_within: Some(ANSWER_WITHIN),
		})
		.await
		.map(drop)
		.map_err(|error| error.to_string());

	assert!(
		federated
			.as_ref()
			.is_err_and(|error| error.contains(NO_ANSWER)),
		"{federated:?}"
	);

	assert!(absent(services, &room_id, user_id.as_str()).await, "the invite was stored");

	Ok(())
}

/// A delayed invite the invitee's server does not answer within 30 seconds
/// ends in the error saying the server did not answer the invite.
///
/// The send answers with that error, nothing is stored, and the finalised
/// record keeps it in place of an event ID.
async fn delayed_stalled_invite(client: &Client<'_>, peer: &Peer) -> Result {
	let room_id = client.create_room(&json!({})).await?;
	let user_id = format!("{STALLED}{}", peer.name);
	let delay_id = schedule_invite(client, &room_id, &user_id).await?;
	let response = send_delayed(client, &delay_id).await?;
	let error = answered(response, StatusCode::INTERNAL_SERVER_ERROR).await?;
	let record = delayed(client, &delay_id).await?;
	let finalised = &record["finalised"];
	let stalled = |error: &Value| {
		error["error"]
			.as_str()
			.is_some_and(|message| message.contains(&format!("{NO_ANSWER} within 30s")))
	};

	assert!(stalled(&error), "{error}");
	assert!(stalled(&finalised["error"]), "{record}");
	assert_eq!(finalised.get("event_id"), None, "{record}");
	assert!(absent(client.services, &room_id, &user_id).await, "the invite was stored");

	Ok(())
}

/// A non-admin's member invite sent as state is refused while invites are
/// blocked.
///
/// The block covers the state route as it covers `/invite` and `createRoom`,
/// since that route would otherwise let a non-admin invite anyone.
async fn non_admin_invite_refused(services: &Services, base: &str) -> Result {
	register(services, "alice", ALICE_TOKEN).await?;

	let bob = register(services, "bob", BOB_TOKEN).await?;
	let bob = bob.as_str();
	let client = Client { services, base, token: ALICE_TOKEN };
	let room_id = client.create_room(&json!({})).await?;
	let body = json!({ "membership": "invite" });
	let response = put_invite(&client, &room_id, bob, "", &body).await?;

	refused(response, services, &room_id, bob).await
}

/// Sends a member invite as state through the client API.
///
/// The query string, if any, rides along, and the status stays the caller's to
/// judge.
async fn put_invite(
	client: &Client<'_>,
	room_id: &RoomId,
	user_id: &str,
	query: &str,
	body: &Value,
) -> Result<Response> {
	let path = format!("rooms/{room_id}/state/m.room.member/{user_id}{query}");

	client
		.request(Method::PUT, &client.url(&path), body)
		.await
}

/// Asserts a request was answered with the expected status.
///
/// The body comes back for the case to inspect, and the assert prints it when
/// the status differs.
async fn answered(response: Response, expected: StatusCode) -> Result<Value> {
	let status = response.status();
	let body: Value = response.json().await?;

	assert_eq!(status, expected, "{body}");

	Ok(body)
}

/// Whether the user has no membership event in the room.
///
/// It reports true only when the room's member state for that user is not
/// found; any other outcome, a failed read included, counts as present.
async fn absent(services: &Services, room_id: &RoomId, user_id: &str) -> bool {
	services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomMember, user_id)
		.await
		.is_missing()
}

/// Takes the latest invite the stub was sent.
///
/// Each case takes every invite it caused, so an empty log means its invite
/// never reached the stub.
fn received(peer: &Peer) -> Result<Value> {
	peer.invites
		.lock()
		.map_err(|e| err!("invite log poisoned: {e}"))?
		.pop()
		.ok_or_else(|| err!("no invite reached the peer"))
}

/// Asserts the state route answered 200 and stored the invite it names.
///
/// The stored invite is returned for the case to inspect further.
async fn invited(
	response: Response,
	services: &Services,
	room_id: &RoomId,
	user_id: &str,
) -> Result<PduEvent> {
	let body = answered(response, StatusCode::OK).await?;
	let stored = services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomMember, user_id)
		.await?;

	assert_eq!(body["event_id"], stored.event_id().as_str());
	assert_eq!(stored.get_content_as_value()["membership"], "invite");

	Ok(stored)
}

/// Asserts a request was refused as forbidden with nothing stored.
///
/// The invitee then has no membership in the room at all.
async fn refused(
	response: Response,
	services: &Services,
	room_id: &RoomId,
	user_id: &str,
) -> Result {
	let body = answered(response, StatusCode::FORBIDDEN).await?;

	assert_eq!(body["errcode"], "M_FORBIDDEN", "{body}");
	assert!(absent(services, room_id, user_id).await, "the invite was stored");

	Ok(())
}

/// Schedules a member invite through the state route's delay parameter.
///
/// The delay outlasts the case, so only an explicit send releases the invite.
async fn schedule_invite(client: &Client<'_>, room_id: &RoomId, user_id: &str) -> Result<String> {
	let query = format!("?{DELAY_PARAM}={LONG_DELAY}");
	let body = json!({ "membership": "invite" });
	let response = put_invite(client, room_id, user_id, &query, &body).await?;
	let reply = accepted(response).await?;

	field(&reply, "delay_id").map(ToOwned::to_owned)
}

/// Sends a scheduled event now.
///
/// The status stays the caller's to judge, since a refused send answers with
/// the refusal.
async fn send_delayed(client: &Client<'_>, delay_id: &str) -> Result<Response> {
	let url = client.client_url(DELAY_PREFIX, &format!("delayed_events/{delay_id}/send"));

	client.post_url(&url, &json!({})).await
}

/// Reads a scheduled event's record, finalised or not.
///
/// A finalised record names the event it sent or the error that ended it.
async fn delayed(client: &Client<'_>, delay_id: &str) -> Result<Value> {
	let url = client.client_url(DELAY_PREFIX, &format!("delayed_events/{delay_id}"));

	accepted(client.get_url(&url).await?).await
}
