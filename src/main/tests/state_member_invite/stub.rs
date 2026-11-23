//! The remote invite cases federate with this stub server.
//!
//! The invitee's localpart selects how it answers an invite. It also serves its
//! key document, answers notary queries and signs as a room's policy server.

use std::{
	io,
	net::TcpListener,
	path::PathBuf,
	sync::{
		Arc, Mutex,
		atomic::{AtomicUsize, Ordering},
	},
	time::Duration,
};

use axum::{
	Json, Router,
	body::Bytes,
	extract::State,
	http::{StatusCode, Uri},
	routing::{get, post},
};
use axum_server::{from_tcp_rustls, tls_rustls::RustlsConfig};
use serde_json::{Value, from_slice, from_value, json};
use tokio::{
	sync::Barrier,
	task::JoinSet,
	time::{sleep, timeout},
};
use tuwunel_core::{
	Result, err, implement,
	ruma::{
		CanonicalJsonObject, CanonicalJsonValue, OwnedServerName, RoomVersionId, ServerName,
		signatures::Ed25519KeyPair,
	},
};
use tuwunel_matrix::{pdu::server_signatures, room_version::rules};
use tuwunel_service::Services;

use super::{
	DECOY, FORGED, POLICY, POLICY_KEY_ID, REFUSED, RETRIED, STALLED, UNSIGNED,
	server_keys::{key_document, key_pair, signed_event},
};

pub(super) type Stub = JoinSet<io::Result<()>>;

const CERTIFICATE: &str = "../../nix/pkgs/complement/certificate.crt";
const PRIVATE_KEY: &str = "../../nix/pkgs/complement/private_key.key";
const INVITE_PATH: &str = "/_matrix/federation/v2/invite/";
const KEYS_PATH: &str = "/_matrix/key/v2/server";
const QUERY_PATH: &str = "/_matrix/key/v2/query/{server}";
const SIGN_PATH: &str = "/_matrix/policy/v1/sign";
const KEY_VERSION: &str = "stub";
const UNPUBLISHED_VERSION: &str = "unpublished";
const POLICY_VERSION: &str = "policy_server";

// Outlasts the delayed send's 30-second invite bound.
const STALL: Duration = Duration::from_secs(32);

// Bounds the wait for a retry's invite, so a send never retried is still
// answered.
const RETRY_WINDOW: Duration = Duration::from_secs(10);

/// A server that keeps each invite it was sent and answers as the invitee's
/// localpart selects.
///
/// It refuses an invite of its `refused` user, returns one of its `unsigned`
/// user unchanged, and signs one of its `forged` user under its published key
/// ID with another key. It answers one of its `stalled` user only after
/// `STALL`, and holds one of its `retried` user until a second arrives, so the
/// two answers return together. It signs an invite to its `policy` user with
/// the key its key document serves alone, and any other invite with that key
/// plus a second signature under a key ID it does not publish. Every other
/// federation request gets an empty success.
///
/// It also counts the fetches of its key document. Asked as a notary, it lists
/// a decoy's document, publishing its key ID under its `forger` key, before
/// its own. As a room's policy server it signs events with its `policy` key
/// under the default room version's rules.
pub(super) struct Peer {
	pub(super) name: OwnedServerName,
	key: Ed25519KeyPair,
	forger: Ed25519KeyPair,
	unpublished: Ed25519KeyPair,
	pub(super) policy: Ed25519KeyPair,
	document: CanonicalJsonObject,
	decoy: CanonicalJsonObject,
	room_version: RoomVersionId,
	pub(super) key_fetches: AtomicUsize,
	pub(super) invites: Mutex<Vec<Value>>,
	retries: Barrier,
}

/// Starts the stub on the listener under a fresh key.
///
/// Its server name is the listener's loopback address, the name its key
/// document and countersignatures carry.
pub(super) async fn serve(
	services: &Services,
	listener: TcpListener,
) -> Result<(Arc<Peer>, Stub)> {
	let name = ServerName::parse(format!("127.0.0.1:{}", listener.local_addr()?.port()))?;
	let key = key_pair(KEY_VERSION)?;
	let forger = key_pair(KEY_VERSION)?;
	let room_version = services
		.server
		.config
		.rooms
		.default_version
		.clone();

	let peer = Arc::new(Peer {
		document: key_document(&name, &key)?,
		decoy: key_document(<&ServerName>::try_from(DECOY)?, &forger)?,
		unpublished: key_pair(UNPUBLISHED_VERSION)?,
		policy: key_pair(POLICY_VERSION)?,
		name,
		key,
		forger,
		room_version,
		key_fetches: AtomicUsize::default(),
		invites: Mutex::default(),
		retries: Barrier::new(2),
	});

	listener.set_nonblocking(true)?;

	let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
	let config =
		RustlsConfig::from_pem_file(manifest.join(CERTIFICATE), manifest.join(PRIVATE_KEY))
			.await?;

	let app = Router::new()
		.route(KEYS_PATH, get(key_server))
		.route(QUERY_PATH, get(notary))
		.route(SIGN_PATH, post(policy_sign))
		.fallback(answer)
		.with_state(peer.clone());

	let remote = from_tcp_rustls(listener, config)?.serve(app.into_make_service());
	let mut stub = Stub::new(); // The set aborts the stub when the exercise drops it.

	stub.spawn_on(remote, services.server.runtime());

	Ok((peer, stub))
}

/// Serves the stub's key document, counting each fetch.
///
/// This server fetches it to verify the stub's countersignature on an invite.
async fn key_server(State(peer): State<Arc<Peer>>) -> Json<CanonicalJsonObject> {
	peer.key_fetches.fetch_add(1, Ordering::Relaxed);

	Json(peer.document.clone())
}

/// Answers a notary query with the decoy's key document before the stub's.
///
/// Every query gets both, whichever server it names.
async fn notary(State(peer): State<Arc<Peer>>) -> Json<Value> {
	Json(json!({ "server_keys": [peer.decoy, peer.document] }))
}

/// Signs an event as the room's policy server.
///
/// An event the stub cannot sign gets a server error naming why.
async fn policy_sign(
	State(peer): State<Arc<Peer>>,
	Json(event): Json<CanonicalJsonObject>,
) -> (StatusCode, Json<Value>) {
	let name = peer.name.as_str();

	match peer.policy_signature(event) {
		| Ok(signature) => (StatusCode::OK, Json(json!({ name: { POLICY_KEY_ID: signature } }))),
		| Err(error) =>
			(StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": error.to_string() }))),
	}
}

/// Signs an event with the stub's policy key and returns that signature.
///
/// It covers the event's redacted form under the default room version's rules,
/// the version every case's room is created with.
#[implement(Peer)]
fn policy_signature(&self, event: CanonicalJsonObject) -> Result<CanonicalJsonValue> {
	let rules = rules(&self.room_version)?;
	let signed = signed_event(event, &self.name, &self.policy, &rules.redaction)?;

	server_signatures(&signed, &self.name)
		.and_then(|signatures| signatures.get(POLICY_KEY_ID))
		.cloned()
		.ok_or_else(|| err!("no policy signature on the signed event"))
}

/// Records an invite and answers it as its invitee's localpart selects.
///
/// Any other federation request gets an empty success.
async fn answer(
	State(peer): State<Arc<Peer>>,
	uri: Uri,
	body: Bytes,
) -> (StatusCode, Json<Value>) {
	if !uri.path().starts_with(INVITE_PATH) {
		return (StatusCode::OK, Json(json!({ "pdus": {} })));
	}

	let invite: Value = from_slice(&body).unwrap_or_default();
	let invitee = invite["event"]["state_key"]
		.as_str()
		.unwrap_or_default();

	if invitee.starts_with(STALLED) {
		sleep(STALL).await;
	}

	if invitee.starts_with(RETRIED) {
		timeout(RETRY_WINDOW, peer.retries.wait())
			.await
			.ok();
	}

	let answer = match invitee {
		| _ if invitee.starts_with(UNSIGNED) =>
			(StatusCode::OK, Json(json!({ "event": invite["event"] }))),
		| _ if invitee.starts_with(FORGED) => peer.countersign(&invite, &[&peer.forger]),
		| _ if invitee.starts_with(POLICY) => peer.countersign(&invite, &[&peer.key]),
		| _ if invitee.starts_with(REFUSED) => {
			let refusal =
				json!({ "errcode": "M_FORBIDDEN", "error": "Invites are refused here." });

			(StatusCode::FORBIDDEN, Json(refusal))
		},
		| _ => peer.countersign(&invite, &[&peer.key, &peer.unpublished]),
	};

	if let Ok(mut invites) = peer.invites.lock() {
		invites.push(invite);
	}

	answer
}

/// Answers an invite with its event signed by the stub under the given keys.
///
/// An invite the stub cannot sign gets a server error naming why, which the
/// case then reports.
#[implement(Peer)]
fn countersign(&self, invite: &Value, keys: &[&Ed25519KeyPair]) -> (StatusCode, Json<Value>) {
	match self.countersigned(invite, keys) {
		| Ok(event) => (StatusCode::OK, Json(json!({ "event": event }))),
		| Err(error) =>
			(StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": error.to_string() }))),
	}
}

/// Returns the invite's event with the stub's signature added under each key.
///
/// Each signature covers the event's redacted form under the rules of the room
/// version the invite names, as an invitee's server signs an invite it accepts.
#[implement(Peer)]
fn countersigned(&self, invite: &Value, keys: &[&Ed25519KeyPair]) -> Result<CanonicalJsonObject> {
	let room_version: RoomVersionId = from_value(invite["room_version"].clone())?;
	let rules = rules(&room_version)?;
	let event = from_value(invite["event"].clone())?;

	keys.iter()
		.try_fold(event, |event, key| signed_event(event, &self.name, key, &rules.redaction))
}
