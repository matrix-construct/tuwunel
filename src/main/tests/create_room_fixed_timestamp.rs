#![cfg(test)]

use std::net::TcpListener;

use futures::future::join;
use reqwest::StatusCode;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result, err,
	ruma::{CanonicalJsonObject, CanonicalJsonValue, RoomId, events::StateEventType},
	utils::BoolExt,
};
use tuwunel_matrix::Event;
use tuwunel_service::{Services, rooms::timeline::Collision};

use self::client::{Client, field, register, wait_until_ready};

mod client;

const TOKEN: &str = "create-room-fixed-timestamp-test-access-token";

// A past timestamp no server-chosen create event carries.
const TIMESTAMP: u64 = 1_700_000_000_000;

// The year 3000, past any allowance for clock skew.
const FUTURE: u64 = 32_503_680_000_000;

/// Drives createRoom with a client-chosen create timestamp and expected ID.
///
/// The timestamp is stored on the create event, and one ahead of the clock is
/// refused. A second create with the same inputs hashes to the same room, so
/// it is refused and the first room is left alone; an expected ID the hash
/// disagrees with is refused before anything is stored. A server-owned create
/// that lands on a taken ID is rehashed with a nonce instead.
#[test]
fn a_fixed_create_timestamp_is_honoured_and_never_reused() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();

	let args = Args::default_test(&["fresh", "cleanup"])
		.with_loopback_listener(port)
		.with_option("listening=true");

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let driven = async {
			let outcome = exercise(&services, &base).await;
			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (served, outcome) = join(async_run(&server), driven).await;

		drop(services);
		async_stop(&server).await?;
		served?;

		outcome
	});

	drop(runtime);

	result
}

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;

	register(services, "fixedts", TOKEN).await?;
	let client = Client { services, base, token: TOKEN };
	let room_id = client
		.create_room(&json!({ "fi.mau.origin_server_ts": TIMESTAMP }))
		.await?;

	let create = services
		.state_accessor
		.room_state_get(&room_id, &StateEventType::RoomCreate, "")
		.await?;

	let stored = u64::from(create.origin_server_ts);

	BoolExt::ok_or_else(stored == TIMESTAMP, || {
		err!("the create event carries {stored}, not the requested {TIMESTAMP}")
	})?;

	let latest = services
		.timeline
		.latest_pdu_in_room(&room_id)
		.await?
		.event_id;

	// The unprefixed spellings are the typed fields the prefixed ones alias.
	let repeated = json!({ "origin_server_ts": TIMESTAMP, "room_id": room_id });

	expect_refusal(&client, &repeated, "M_ROOM_IN_USE").await?;

	let same_timestamp = json!({ "fi.mau.origin_server_ts": TIMESTAMP });

	expect_refusal(&client, &same_timestamp, "M_ROOM_IN_USE").await?;

	let untouched = services
		.timeline
		.latest_pdu_in_room(&room_id)
		.await?
		.event_id;

	BoolExt::ok_or_else(untouched == latest, || {
		err!("a refused create appended {untouched} to the first room after {latest}")
	})?;

	let later = TIMESTAMP.saturating_add(1);
	let mismatched = json!({ "fi.mau.origin_server_ts": later, "fi.mau.room_id": "!notthehash" });

	expect_refusal(&client, &mismatched, "M_INVALID_PARAM").await?;

	// The refused create stored nothing, or this identical one would collide.
	client
		.create_room(&json!({ "fi.mau.origin_server_ts": later }))
		.await?;

	let future = json!({ "fi.mau.origin_server_ts": FUTURE });

	expect_refusal(&client, &future, "M_INVALID_PARAM").await?;

	retried_collision(services, &room_id).await
}

/// Rebuild the first room's create event as a server-owned create.
///
/// Its inputs hash to the taken ID, which a server-chosen timestamp reaches
/// only by chance, so the retry is driven through the same service call room
/// creation and upgrades make.
#[tracing::instrument(level = "debug", skip_all)]
async fn retried_collision(services: &Services, room_id: &RoomId) -> Result {
	let create = services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomCreate, "")
		.await?;

	let latest = services
		.timeline
		.latest_pdu_in_room(room_id)
		.await?
		.event_id;

	let content: CanonicalJsonObject = create.get_content()?;
	let timestamp = Some(create.origin_server_ts());
	let retried = services
		.timeline
		.build_and_append_create_pdu_with(content, create.sender(), timestamp, Collision::Retry)
		.await?;

	let nonce = services
		.timeline
		.get_pdu(&retried)
		.await?
		.get_content::<CanonicalJsonObject>()?
		.remove("uwu.woof");

	let untouched = services
		.timeline
		.latest_pdu_in_room(room_id)
		.await?
		.event_id;

	BoolExt::ok_or_else(retried.as_str() != create.event_id().as_str(), || {
		err!("a retried create reused the taken ID {retried}")
	})?;

	BoolExt::ok_or_else(matches!(nonce, Some(CanonicalJsonValue::String(_))), || {
		err!("the retried create carries no nonce: {nonce:?}")
	})?;

	BoolExt::ok_or_else(untouched == latest, || {
		err!("a retried create appended {untouched} to the first room after {latest}")
	})
}

#[tracing::instrument(level = "debug", skip_all)]
async fn expect_refusal(client: &Client<'_>, body: &Value, errcode: &str) -> Result {
	let response = client
		.post_url(&client.url("createRoom"), body)
		.await?;

	let status = response.status();
	let reply: Value = response.json().await?;
	let refused = status == StatusCode::BAD_REQUEST && field(&reply, "errcode")? == errcode;

	BoolExt::ok_or_else(refused, || err!("{body} answered {status} {reply}, not {errcode}"))
}
