#![cfg(test)]

use std::{
	convert::identity,
	fs::remove_dir_all,
	net::TcpListener,
	path::PathBuf,
	sync::{
		Arc,
		atomic::{AtomicUsize, Ordering::SeqCst},
	},
	time::Duration,
};

use axum::{
	Json, Router,
	extract::State,
	http::{StatusCode, Uri},
	response::{IntoResponse, Response},
	routing::any,
};
use axum_server::{from_tcp_rustls, tls_rustls::RustlsConfig};
use futures::future::join;
use serde_json::{Value, json, value::RawValue as RawJsonValue};
use serde_urlencoded::from_str;
use tokio::{
	spawn,
	time::{sleep, timeout},
};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	PduCount, Result, err,
	pdu::PduBuilder,
	ruma::{
		MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedServerName, RoomId,
		RoomVersionId, ServerName, UInt, UserId,
		events::room::{
			member::{MembershipState, RoomMemberEventContent},
			message::RoomMessageEventContent,
		},
	},
};
use tuwunel_service::{Services, rooms::state_cache::MembershipUpdate, users::Register};

const CERTIFICATE: &str = "../../nix/pkgs/complement/certificate.crt";
const PRIVATE_KEY: &str = "../../nix/pkgs/complement/private_key.key";
const EDGE_TS: u64 = 100;
const MISMATCH_TS: u64 = 120;
const CLIENT_MISMATCH_QUERY_TS: u64 = 10;
const ADMIN_MISMATCH_QUERY_TS: u64 = 20;
const DIRECTION_QUERY_TS: u64 = 50;
const CLIENT_OMISSION_QUERY_TS: u64 = 75;
const CLIENT_DECOY_TS: u64 = 80;
const ADMIN_OMISSION_QUERY_TS: u64 = 65;
const ADMIN_DECOY_TS: u64 = 70;
const REMOTE_QUERY_TS: u64 = 55;
const REMOTE_TS: u64 = 60;
const TIMEOUT: Duration = Duration::from_secs(20);

struct DatabasePath(PathBuf);

struct PeerState {
	origin: OwnedServerName,
	existing: OwnedEventId,
	existing_ts: u64,
	mismatch: OwnedEventId,
	client_missing: OwnedEventId,
	admin_missing: OwnedEventId,
	client_decoy: OwnedEventId,
	remote: OwnedEventId,
	remote_ts: u64,
	foreign: OwnedEventId,
	foreign_ts: u64,
	pdu: Box<RawJsonValue>,
	client_decoy_pdu: Box<RawJsonValue>,
	admin_decoy_pdu: Box<RawJsonValue>,
	remote_pdu: Box<RawJsonValue>,
	timestamps: AtomicUsize,
	backfills: AtomicUsize,
}

enum Expected {
	NotFound,
	Local,
	ClientDecoy,
	Remote,
}

struct Case<'a> {
	token: &'a str,
	path: &'a str,
	ts: u64,
	dir: &'a str,
	requests: usize,
	label: &'a str,
	expected: Expected,
}

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

#[test]
fn timestamp_fallback_confirms_the_requested_event() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let peer_listener = TcpListener::bind(("127.0.0.1", 0))?;
	let peer_address = format!("127.0.0.1:{}", peer_listener.local_addr()?.port());
	let peer = ServerName::parse(&peer_address)?;

	peer_listener.set_nonblocking(true)?;

	let db_path = DatabasePath(Args::test_database_path("timestamp-confirmation"));

	let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
	let certificate = manifest.join(CERTIFICATE);
	let private_key = manifest.join(PRIVATE_KEY);
	let mut args = Args::default_test(&["fresh", "cleanup"]);

	args.option.extend([
		format!("database_path={:?}", db_path.0),
		"address=[\"127.0.0.1\"]".to_owned(),
		format!("port={port}"),
		"listening=true".to_owned(),
		"allow_invalid_tls_certificates=true".to_owned(),
		"ip_range_denylist=[]".to_owned(),
		"federation_loopback=true".to_owned(),
		format!("trusted_servers=[\"{peer_address}\"]"),
		"log=\"error\"".to_owned(),
	]);

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let exercise = async {
			let outcome =
				exercise(&services, &base, peer, peer_listener, certificate, private_key).await;

			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (run_result, outcome) = join(async_run(&server), exercise).await;

		drop(services);
		async_stop(&server).await?;
		run_result?;

		outcome
	});

	drop(runtime);
	result
}

async fn exercise(
	services: &Services,
	base: &str,
	peer: OwnedServerName,
	peer_listener: TcpListener,
	certificate: PathBuf,
	private_key: PathBuf,
) -> Result {
	wait_until_ready(services, base).await?;

	let user_id =
		UserId::parse_with_server_name("timestamp-confirmation", services.globals.server_name())?;

	let token = "timestamp-confirmation-token-000000000001";
	let admin_token = "timestamp-confirmation-admin-token-00000001";

	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password: Some("timestamp-confirmation-password"),
			..Default::default()
		})
		.await?;

	services
		.users
		.create_device(&user_id, None, (Some(token), None), None, None, None)
		.await?;

	services
		.users
		.create_device(
			&services.globals.server_user,
			None,
			(Some(admin_token), None),
			None,
			None,
			None,
		)
		.await?;

	let room_id = create_room(services, base, token).await?;
	let room_version = services.state.get_room_version(&room_id).await?;
	let (existing, pdu) =
		make_pdu(services, &user_id, &room_id, &room_version, EDGE_TS, "edge").await?;

	services
		.timeline
		.backfill_pdu(&room_id, services.globals.server_name(), pdu.clone())
		.await?;

	let mismatch_ts = UInt::try_from(MISMATCH_TS).expect("test timestamp fits Matrix UInt");
	let mismatch_builder = PduBuilder {
		timestamp: Some(MilliSecondsSinceUnixEpoch(mismatch_ts)),
		..PduBuilder::timeline(&RoomMessageEventContent::text_plain("mismatch"))
	};

	let state_lock = services.state.mutex.lock(&room_id).await;
	let mismatch = services
		.timeline
		.build_and_append_pdu(mismatch_builder, &user_id, &room_id, &state_lock)
		.await?;

	drop(state_lock);

	let (client_decoy, client_decoy_pdu) =
		make_pdu(services, &user_id, &room_id, &room_version, CLIENT_DECOY_TS, "client decoy")
			.await?;

	let (_, admin_decoy_pdu) =
		make_pdu(services, &user_id, &room_id, &room_version, ADMIN_DECOY_TS, "admin decoy")
			.await?;

	let (remote, remote_pdu) =
		make_pdu(services, &user_id, &room_id, &room_version, REMOTE_TS, "remote").await?;

	let foreign_room_id = create_room(services, base, token).await?;
	let foreign_ts = 8_000_000_000_000_u64
		.try_into()
		.expect("foreign timestamp fits Matrix UInt");

	let foreign_ts = MilliSecondsSinceUnixEpoch(foreign_ts);

	let foreign_builder = PduBuilder {
		timestamp: Some(foreign_ts),
		..PduBuilder::timeline(&RoomMessageEventContent::text_plain("foreign"))
	};

	let state_lock = services.state.mutex.lock(&foreign_room_id).await;
	let foreign = services
		.timeline
		.build_and_append_pdu(foreign_builder, &user_id, &foreign_room_id, &state_lock)
		.await?;

	drop(state_lock);

	let client_missing = format!("$timestamp-client-missing:{peer}").try_into()?;
	let admin_missing = format!("$timestamp-admin-missing:{peer}").try_into()?;
	let counter = || AtomicUsize::new(0);
	let state = PeerState {
		origin: peer.clone(),
		existing,
		existing_ts: EDGE_TS,
		mismatch,
		client_missing,
		admin_missing,
		client_decoy,
		remote,
		remote_ts: REMOTE_TS,
		foreign,
		foreign_ts: u64::from(foreign_ts.0),
		pdu,
		client_decoy_pdu,
		admin_decoy_pdu,
		remote_pdu,
		timestamps: counter(),
		backfills: counter(),
	};

	let state = Arc::new(state);

	let stub = spawn(serve_peer(peer_listener, state.clone(), certificate, private_key));

	let remote_user = UserId::parse_with_server_name("timestamp", &peer)?;

	services
		.state_cache
		.update_membership(MembershipUpdate {
			room_id: &room_id,
			user_id: &remote_user,
			membership_event: RoomMemberEventContent::new(MembershipState::Join),
			sender: &remote_user,
			last_state: None,
			invite_via: None,
			update_joined_count: true,
			count: PduCount::Normal(*services.globals.next_count()),
		})
		.await?;

	let outcome =
		timeout(TIMEOUT, exercise_routes(services, base, &room_id, token, admin_token, &state))
			.await
			.map_err(|_| err!("timestamp route exercise timed out"))
			.and_then(identity);

	match stub.is_finished() {
		| true => stub
			.await
			.unwrap_or_else(|error| Err(err!("the peer stub panicked: {error}")))
			.and(outcome),
		| false => {
			stub.abort();
			outcome
		},
	}
}

async fn wait_until_ready(services: &Services, base: &str) -> Result {
	for _ in 0..500 {
		if services
			.client
			.clients
			.default
			.get(format!("{base}/_matrix/client/versions"))
			.send()
			.await
			.is_ok()
		{
			return Ok(());
		}

		sleep(Duration::from_millis(20)).await;
	}

	Err(err!("server listener did not become ready"))
}

async fn create_room(services: &Services, base: &str, token: &str) -> Result<OwnedRoomId> {
	let response = services
		.client
		.clients
		.default
		.post(format!("{base}/_matrix/client/v3/createRoom"))
		.bearer_auth(token)
		.json(&json!({}))
		.send()
		.await?
		.error_for_status()?
		.json::<Value>()
		.await?;

	let room_id = response
		.get("room_id")
		.and_then(Value::as_str)
		.ok_or_else(|| err!("createRoom response omitted room_id"))?;

	Ok(room_id.try_into()?)
}

async fn make_pdu(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	room_version: &RoomVersionId,
	ts: u64,
	body: &str,
) -> Result<(OwnedEventId, Box<RawJsonValue>)> {
	let ts = UInt::try_from(ts).expect("test timestamp fits Matrix UInt");
	let builder = PduBuilder {
		timestamp: Some(MilliSecondsSinceUnixEpoch(ts)),
		..PduBuilder::timeline(&RoomMessageEventContent::text_plain(body))
	};

	let state_lock = services.state.mutex.lock(room_id).await;
	let (event, event_json) = services
		.timeline
		.create_hash_and_sign_event(builder, user_id, room_id, &state_lock)
		.await?;

	drop(state_lock);

	let pdu = services
		.federation
		.format_pdu_into(event_json, Some(room_version))
		.await;

	Ok((event.event_id, pdu))
}

async fn serve_peer(
	listener: TcpListener,
	state: Arc<PeerState>,
	certificate: PathBuf,
	private_key: PathBuf,
) -> Result {
	let config = RustlsConfig::from_pem_file(certificate, private_key).await?;
	let app = Router::new()
		.route("/_matrix/federation/{*rest}", any(answer_peer))
		.with_state(state);

	from_tcp_rustls(listener, config)?
		.serve(app.into_make_service())
		.await?;

	Ok(())
}

async fn answer_peer(uri: Uri, State(state): State<Arc<PeerState>>) -> Response {
	if uri.path().contains("/timestamp_to_event/") {
		let Some(query_ts) = query_value(&uri, "ts").and_then(|ts| ts.parse::<u64>().ok()) else {
			return (StatusCode::BAD_REQUEST, "timestamp query omitted ts").into_response();
		};

		let Some(dir) = query_value(&uri, "dir") else {
			return (StatusCode::BAD_REQUEST, "timestamp query omitted dir").into_response();
		};

		let expected_dir = if query_ts == DIRECTION_QUERY_TS { "b" } else { "f" };

		if dir != expected_dir {
			return (StatusCode::BAD_REQUEST, "timestamp query used an unexpected dir")
				.into_response();
		}

		state.timestamps.fetch_add(1, SeqCst);

		let event_id = match query_ts {
			| ts if ts == state.foreign_ts => &state.foreign,
			| CLIENT_MISMATCH_QUERY_TS | ADMIN_MISMATCH_QUERY_TS | DIRECTION_QUERY_TS =>
				&state.mismatch,
			| CLIENT_OMISSION_QUERY_TS => &state.client_missing,
			| ADMIN_OMISSION_QUERY_TS => &state.admin_missing,
			| REMOTE_QUERY_TS => &state.remote,
			| _ =>
				return (StatusCode::BAD_REQUEST, "unexpected timestamp query").into_response(),
		};

		let origin_server_ts = match query_ts {
			| DIRECTION_QUERY_TS => MISMATCH_TS,
			| CLIENT_OMISSION_QUERY_TS => CLIENT_DECOY_TS,
			| ADMIN_OMISSION_QUERY_TS => ADMIN_DECOY_TS,
			| REMOTE_QUERY_TS => state.remote_ts,
			| _ => query_ts,
		};

		let payload = json!({
			"event_id": event_id,
			"origin_server_ts": origin_server_ts,
		});

		return Json(payload).into_response();
	}

	if uri.path().contains("/backfill/") {
		let Some(anchor) = query_value(&uri, "v") else {
			return (StatusCode::BAD_REQUEST, "backfill query omitted v").into_response();
		};

		if anchor != state.mismatch.as_str()
			&& anchor != state.client_missing.as_str()
			&& anchor != state.admin_missing.as_str()
			&& anchor != state.remote.as_str()
			&& anchor != state.foreign.as_str()
		{
			return (StatusCode::BAD_REQUEST, "backfill query used an unexpected anchor")
				.into_response();
		}

		state.backfills.fetch_add(1, SeqCst);
		let pdus = match anchor.as_str() {
			| anchor if anchor == state.client_missing.as_str() =>
				json!([&state.client_decoy_pdu]),
			| anchor if anchor == state.admin_missing.as_str() => json!([&state.admin_decoy_pdu]),
			| anchor if anchor == state.remote.as_str() => json!([{}, &state.remote_pdu]),
			| _ => json!([&state.pdu]),
		};

		let payload = json!({
			"origin": state.origin.as_str(),
			"origin_server_ts": 0,
			"pdus": pdus,
		});

		return Json(payload).into_response();
	}

	(StatusCode::NOT_FOUND, "unexpected federation request").into_response()
}

fn query_value(uri: &Uri, name: &str) -> Option<String> {
	from_str::<Vec<(String, String)>>(uri.query()?)
		.ok()?
		.into_iter()
		.find_map(|(key, value)| (key == name).then_some(value))
}

async fn exercise_routes(
	services: &Services,
	base: &str,
	room_id: &RoomId,
	token: &str,
	admin_token: &str,
	peer: &PeerState,
) -> Result {
	let client_path = format!("/_matrix/client/v1/rooms/{room_id}/timestamp_to_event");
	let admin_path = format!("/_synapse/admin/v1/rooms/{room_id}/timestamp_to_event");

	assert_case(services, base, peer, Case {
		token,
		path: &client_path,
		ts: CLIENT_MISMATCH_QUERY_TS,
		dir: "f",
		requests: 1,
		label: "client mismatch preserves local",
		expected: Expected::Local,
	})
	.await?;

	assert_case(services, base, peer, Case {
		token: admin_token,
		path: &admin_path,
		ts: ADMIN_MISMATCH_QUERY_TS,
		dir: "f",
		requests: 2,
		label: "admin mismatch preserves local",
		expected: Expected::Local,
	})
	.await?;

	assert_case(services, base, peer, Case {
		token,
		path: &client_path,
		ts: DIRECTION_QUERY_TS,
		dir: "b",
		requests: 3,
		label: "client direction mismatch",
		expected: Expected::NotFound,
	})
	.await?;

	assert_case(services, base, peer, Case {
		token,
		path: &client_path,
		ts: CLIENT_OMISSION_QUERY_TS,
		dir: "f",
		requests: 4,
		label: "client omission preserves local",
		expected: Expected::Local,
	})
	.await?;

	assert_case(services, base, peer, Case {
		token: admin_token,
		path: &admin_path,
		ts: ADMIN_OMISSION_QUERY_TS,
		dir: "f",
		requests: 5,
		label: "admin omission preserves local",
		expected: Expected::ClientDecoy,
	})
	.await?;

	assert_case(services, base, peer, Case {
		token: admin_token,
		path: &admin_path,
		ts: peer.foreign_ts,
		dir: "f",
		requests: 6,
		label: "admin cross-room target",
		expected: Expected::NotFound,
	})
	.await?;

	assert_case(services, base, peer, Case {
		token,
		path: &client_path,
		ts: REMOTE_QUERY_TS,
		dir: "f",
		requests: 7,
		label: "client confirmed remote target",
		expected: Expected::Remote,
	})
	.await?;

	Ok(())
}

async fn assert_case(
	services: &Services,
	base: &str,
	peer: &PeerState,
	case: Case<'_>,
) -> Result {
	let (status, body) =
		timestamp(services, base, case.token, case.path, case.ts, case.dir).await?;

	match case.expected {
		| Expected::NotFound => assert_not_found(status, &body, case.label),
		| Expected::Local =>
			assert_hit(status, &body, &peer.existing, peer.existing_ts, case.label),
		| Expected::ClientDecoy =>
			assert_hit(status, &body, &peer.client_decoy, CLIENT_DECOY_TS, case.label),
		| Expected::Remote => assert_hit(status, &body, &peer.remote, peer.remote_ts, case.label),
	}

	assert_eq!(peer.timestamps.load(SeqCst), case.requests, "{}", case.label);
	assert_eq!(peer.backfills.load(SeqCst), case.requests, "{}", case.label);

	Ok(())
}

async fn timestamp(
	services: &Services,
	base: &str,
	token: &str,
	path: &str,
	ts: u64,
	dir: &str,
) -> Result<(u16, Value)> {
	let response = services
		.client
		.clients
		.default
		.get(format!("{base}{path}?ts={ts}&dir={dir}"))
		.bearer_auth(token)
		.send()
		.await?;

	let status = response.status().as_u16();
	let body = response.json().await?;

	Ok((status, body))
}

fn assert_not_found(status: u16, body: &Value, label: &str) {
	assert_eq!(status, 404, "{label}: {body}");
	assert_eq!(body["errcode"], "M_NOT_FOUND", "{label}: {body}");
}

fn assert_hit(status: u16, body: &Value, event_id: &OwnedEventId, ts: u64, label: &str) {
	assert_eq!(status, StatusCode::OK.as_u16(), "{label}");
	assert_eq!(body.get("event_id").and_then(Value::as_str), Some(event_id.as_str()), "{label}");
	assert_eq!(
		body.get("origin_server_ts")
			.and_then(Value::as_u64),
		Some(ts),
		"{label}"
	);
}
