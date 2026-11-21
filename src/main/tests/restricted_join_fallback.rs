#![cfg(test)]

use std::{
	net::TcpListener,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
};

use axum::{
	Json, Router,
	extract::State,
	http::{StatusCode, Uri},
	response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use tuwunel_core::{
	Result,
	ruma::{OwnedServerName, OwnedUserId, RoomId, ServerName, UserId},
};
use tuwunel_service::Services;

use self::{
	client::{Client, register},
	fixture::boot,
	restricted::restricted,
	stub::{Stub, serve},
};

mod client;
mod fixture;

#[path = "client/restricted.rs"]
mod restricted;

#[path = "fixture/stub.rs"]
mod stub;

const ALICE_TOKEN: &str = "restricted-join-fallback-alice-access-token";
const BOB_TOKEN: &str = "restricted-join-fallback-bob-access-token";
const MAKE_JOIN: &str = "/make_join/";
const SEND_JOIN: &str = "/send_join/";

// The stubs' certificate and loopback addresses pass.
const STUB_OPTIONS: [&str; 2] = [
	"network.http.allow_invalid_tls_certificates=true",
	"network.http.ip_range_denylist=[]",
];

/// A resident server whose join template names one authoriser.
///
/// It answers a make_join with its template and any other request with an
/// empty success, and notes whether a make_join or a send_join ever reached it.
struct Resident {
	name: OwnedServerName,
	template: Value,
	make_join: AtomicBool,
	send_join: AtomicBool,
}

/// A restricted join refused here is not sent on a local user's authority.
///
/// The join reaches a remote resident only after this server refused it, and a
/// template naming a local user as its authoriser ends it with that refusal,
/// unchanged, before any send_join. A template one of the resident's own users
/// authorises is still sent back as a send_join, which shows the stub sees one
/// when it comes.
#[test]
fn local_authority_stays_out_of_remote_join_fallbacks() -> Result {
	boot("restricted-join-fallback", STUB_OPTIONS, exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let alice = register(services, "alice", ALICE_TOKEN).await?;

	register(services, "bob", BOB_TOKEN).await?;

	let owner = Client { services, base, token: ALICE_TOKEN };
	let joiner = Client { services, base, token: BOB_TOKEN };
	let allowed = owner
		.create_room(&json!({ "preset": "private_chat" }))
		.await?;

	let room_id = owner.create_room(&restricted(&allowed)).await?;
	let (local_authority, _local_stub) = resident(services, |_| Ok(alice)).await?;
	let (remote_authority, _remote_stub) =
		resident(services, |name| Ok(UserId::parse_with_server_name("member", name)?)).await?;

	let refusal = join(&joiner, &room_id, None).await?;
	let fallback = join(&joiner, &room_id, Some(&local_authority.name)).await?;

	assert_eq!(refusal.0, StatusCode::FORBIDDEN, "{}", refusal.1);
	assert_eq!(fallback, refusal);
	assert!(
		local_authority.make_join.load(Ordering::Relaxed),
		"the fallback never reached the resident"
	);

	assert!(
		!local_authority.send_join.load(Ordering::Relaxed),
		"a join a local user authorised was sent"
	);

	join(&joiner, &room_id, Some(&remote_authority.name)).await?;

	assert!(
		remote_authority.send_join.load(Ordering::Relaxed),
		"a join the resident's own user authorised was not sent"
	);

	Ok(())
}

/// Starts a resident whose template names the authoriser chosen for its name.
///
/// Its server name is the listener's loopback address. The template carries
/// only what building the join needs, in the room version rooms here default
/// to.
async fn resident<Authoriser>(
	services: &Services,
	authoriser: Authoriser,
) -> Result<(Arc<Resident>, Stub)>
where
	Authoriser: FnOnce(&ServerName) -> Result<OwnedUserId>,
{
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let name = ServerName::parse(format!("127.0.0.1:{}", listener.local_addr()?.port()))?;
	let authoriser = authoriser(&name)?;
	let template = json!({
		"room_version": services.server.config.rooms.default_version,
		"event": {
			"type": "m.room.member",
			"content": {
				"membership": "join",
				"join_authorised_via_users_server": authoriser,
			},
			"prev_events": [],
			"auth_events": [],
			"depth": 1,
		},
	});

	let resident = Arc::new(Resident {
		name,
		template,
		make_join: AtomicBool::default(),
		send_join: AtomicBool::default(),
	});

	let app = Router::new()
		.fallback(answer)
		.with_state(resident.clone());

	let stub = serve(services, listener, app).await?;

	Ok((resident, stub))
}

async fn answer(State(resident): State<Arc<Resident>>, uri: Uri) -> Response {
	let path = uri.path();
	let make_join = path.contains(MAKE_JOIN);

	resident
		.make_join
		.fetch_or(make_join, Ordering::Relaxed);

	resident
		.send_join
		.fetch_or(path.contains(SEND_JOIN), Ordering::Relaxed);

	if make_join {
		return Json(&resident.template).into_response();
	}

	Json(json!({})).into_response()
}

/// Joins the room as the client, through the given server or through none.
///
/// The status and body come back for the case to compare, since a refusal is
/// the outcome under test.
async fn join(
	client: &Client<'_>,
	room_id: &RoomId,
	via: Option<&ServerName>,
) -> Result<(StatusCode, Value)> {
	let path = match via {
		| Some(via) => format!("join/{room_id}?via={via}"),
		| None => format!("join/{room_id}"),
	};

	let response = client
		.post_url(&client.url(&path), &json!({}))
		.await?;

	let status = response.status();
	let body = response.json().await?;

	Ok((status, body))
}
