#![cfg(test)]

use std::{
	io,
	net::TcpListener,
	path::PathBuf,
	sync::{Arc, Mutex},
};

use axum::{
	Json, Router,
	body::Bytes,
	extract::State,
	http::{StatusCode, Uri},
};
use axum_server::{from_tcp_rustls, tls_rustls::RustlsConfig};
use reqwest::{Method, Response};
use serde_json::{Value, json};
use tokio::task::JoinSet;
use tuwunel_core::{
	Result, err,
	result::NotFound,
	ruma::{RoomId, ServerName, events::StateEventType},
};
use tuwunel_matrix::{Event, pdu::PduEvent};
use tuwunel_service::Services;

use self::{
	appservice::{Bridge, register_appservice},
	client::{Client, register},
	fixture::boot,
};

mod appservice;
mod client;
mod fixture;

type Stub = JoinSet<io::Result<()>>;

const CERTIFICATE: &str = "../../nix/pkgs/complement/certificate.crt";
const PRIVATE_KEY: &str = "../../nix/pkgs/complement/private_key.key";
const INVITE_PATH: &str = "/_matrix/federation/v2/invite/";
const REFUSED: &str = "@refused:";
const ALICE_TOKEN: &str = "state-member-invite-alice-access-token";
const BRIDGE_TOKEN: &str = "state-member-invite-bridge-access-token";
const TIMESTAMP: u64 = 1_234_567_890_123;
const QUIET: [&str; 2] = ["logging.enable=false", "database.pool.max_workers=64"];

// The stub's certificate and loopback address pass.
const STUB_OPTIONS: [&str; 2] = [
	"network.http.allow_invalid_tls_certificates=true",
	"network.http.ip_range_denylist=[]",
];

/// A server that keeps each invite it was sent and accepts all but one.
///
/// It refuses an invite of its `refused` user and returns any other unchanged,
/// which this server accepts on its own signature. Every other federation
/// request gets an empty success.
#[derive(Default)]
struct Peer {
	invites: Mutex<Vec<Value>>,
}

#[test]
fn member_invites_sent_as_state() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let federating = QUIET.into_iter().chain(STUB_OPTIONS);

	boot("state-member-invite-remote", federating, async move |services, base| {
		remote_invites(services, base, listener).await
	})
}

/// Member invites sent as state to users on the stub reach it as invites.
///
/// The state route once stored such an invite and answered 200 without asking
/// the invitee's server, which then dropped the copy it got in a transaction
/// while it was not in the room. An accepted invite is now stored as the stub
/// returned it, and a refusal reaches the client with nothing stored.
async fn remote_invites(services: &Services, base: &str, listener: TcpListener) -> Result {
	let server = ServerName::parse(format!("127.0.0.1:{}", listener.local_addr()?.port()))?;
	let (peer, _stub) = serve(services, listener).await?;

	ordinary_invite(services, base, &server, &peer).await?;
	appservice_invite(services, base, &server, &peer).await
}

async fn serve(services: &Services, listener: TcpListener) -> Result<(Arc<Peer>, Stub)> {
	listener.set_nonblocking(true)?;

	let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
	let config =
		RustlsConfig::from_pem_file(manifest.join(CERTIFICATE), manifest.join(PRIVATE_KEY))
			.await?;

	let peer = Arc::new(Peer::default());
	let app = Router::new()
		.fallback(answer)
		.with_state(peer.clone());

	let remote = from_tcp_rustls(listener, config)?.serve(app.into_make_service());
	let mut stub = Stub::new(); // The set aborts the stub when the exercise drops it.

	stub.spawn_on(remote, services.server.runtime());

	Ok((peer, stub))
}

async fn answer(
	State(peer): State<Arc<Peer>>,
	uri: Uri,
	body: Bytes,
) -> (StatusCode, Json<Value>) {
	if !uri.path().starts_with(INVITE_PATH) {
		return (StatusCode::OK, Json(json!({ "pdus": {} })));
	}

	let invite: Value = serde_json::from_slice(&body).unwrap_or_default();
	let event = invite["event"].clone();
	let refused = event["state_key"]
		.as_str()
		.is_some_and(|user_id| user_id.starts_with(REFUSED));

	if let Ok(mut invites) = peer.invites.lock() {
		invites.push(invite);
	}

	if refused {
		let refusal = json!({ "errcode": "M_FORBIDDEN", "error": "Invites are refused here." });

		return (StatusCode::FORBIDDEN, Json(refusal));
	}

	(StatusCode::OK, Json(json!({ "event": event })))
}

/// A user's invite sent as state reaches the invitee's server.
///
/// The invite it saw keeps the client's own content keys and carries the
/// room's create event as invite state. A refused invite stores nothing.
async fn ordinary_invite(
	services: &Services,
	base: &str,
	server: &ServerName,
	peer: &Peer,
) -> Result {
	let alice = register(services, "alice", ALICE_TOKEN).await?;
	let client = Client { services, base, token: ALICE_TOKEN };
	let room_id = client.create_room(&json!({})).await?;
	let bob = format!("@bob:{server}");
	let body = json!({ "membership": "invite", "fi.mau.will_auto_accept": true });
	let response = put_invite(&client, &room_id, &bob, "", &body).await?;
	let stored = invited(response, services, &room_id, &bob).await?;
	let invite = received(peer)?;

	assert_eq!(stored.get_content_as_value()["fi.mau.will_auto_accept"], true);
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

	let refused_user = format!("{REFUSED}{server}");
	let response = put_invite(&client, &room_id, &refused_user, "", &body).await?;

	refused(response, services, &room_id, &refused_user).await?;
	received(peer).map(drop)
}

/// An appservice's invite sent as state reaches the invitee's server.
///
/// The invite it saw names the user the appservice acted as and carries the
/// timestamp the appservice asked for.
async fn appservice_invite(
	services: &Services,
	base: &str,
	server: &ServerName,
	peer: &Peer,
) -> Result {
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

	let carol = format!("@carol:{server}");
	let query = format!("?user_id={bridged}&ts={TIMESTAMP}");
	let body = json!({ "membership": "invite" });
	let response = put_invite(&client, room_id, &carol, &query, &body).await?;

	invited(response, services, room_id, &carol).await?;

	let invite = received(peer)?;

	assert_eq!(invite["event"]["sender"], bridged.as_str());
	assert_eq!(invite["event"]["origin_server_ts"], TIMESTAMP);

	Ok(())
}

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

async fn invited(
	response: Response,
	services: &Services,
	room_id: &RoomId,
	user_id: &str,
) -> Result<PduEvent> {
	assert_eq!(response.status(), StatusCode::OK);

	let body = response.json::<Value>().await?;
	let stored = services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomMember, user_id)
		.await?;

	assert_eq!(body["event_id"], stored.event_id().as_str());
	assert_eq!(stored.get_content_as_value()["membership"], "invite");

	Ok(stored)
}

fn received(peer: &Peer) -> Result<Value> {
	peer.invites
		.lock()
		.map_err(|e| err!("invite log poisoned: {e}"))?
		.pop()
		.ok_or_else(|| err!("no invite reached the peer"))
}

async fn refused(
	response: Response,
	services: &Services,
	room_id: &RoomId,
	user_id: &str,
) -> Result {
	assert_eq!(response.status(), StatusCode::FORBIDDEN);
	assert_eq!(response.json::<Value>().await?["errcode"], "M_FORBIDDEN");
	assert!(
		services
			.state_accessor
			.room_state_get(room_id, &StateEventType::RoomMember, user_id)
			.await
			.is_not_found()
	);

	Ok(())
}
