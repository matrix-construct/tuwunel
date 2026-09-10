#![cfg(test)]

use std::net::TcpListener;

use futures::future::join;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result, err,
	ruma::{RoomId, UserId},
};
use tuwunel_service::Services;

use self::client::{Client, field, register, wait_until_ready};

mod client;

const ACCESS_TOKEN: &str = "sync-full-state-test-access-token";
const EMPTY_TIMELINE: &str = r#"{"room":{"timeline":{"limit":0}}}"#;

// Full-state responses carry quiet rooms even when the timeline is empty.
#[test]
fn full_state_includes_quiet_joined_rooms() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={port}"))
		.with_option("listening=true");

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let exercise = async {
			let outcome = exercise(&services, &base).await;
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

async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;

	let user = register(services, "fullstate", ACCESS_TOKEN).await?;
	let client = Client { services, base, token: ACCESS_TOKEN };
	let plain = client
		.create_room(&json!({"preset": "private_chat"}))
		.await?;

	let encrypted = client
		.create_room(&json!({"preset": "private_chat"}))
		.await?;

	let before = sync(&client, &[]).await?;
	let before = field(&before, "next_batch")?;

	// Make these the last events in their rooms: state-before-last-event
	// would lose the topic or encryption, with no timeline event to restore it.
	for (room, event_type, content) in [
		(&plain, "m.room.topic", json!({"topic": "quiet plain room"})),
		(&encrypted, "m.room.encryption", json!({"algorithm": "m.megolm.v1.aes-sha2"})),
	] {
		services
			.client
			.clients
			.default
			.put(client.url(&format!("rooms/{room}/state/{event_type}")))
			.bearer_auth(ACCESS_TOKEN)
			.json(&content)
			.send()
			.await?
			.error_for_status()?;
	}

	let initial = sync(&client, &[]).await?;
	let since = field(&initial, "next_batch")?;
	let incremental = sync(&client, &[("since", since)]).await?;

	for room in [&plain, &encrypted] {
		if incremental["rooms"]["join"]
			.get(room.as_str())
			.is_some()
		{
			return Err!("ordinary incremental sync included a quiet room");
		}
	}

	let unstable_query = [
		("since", since),
		("full_state", "true"),
		("org.matrix.msc4222.use_state_after", "true"),
	];

	for (case, query, state_field) in [
		(
			"quiet full state",
			[("since", since), ("full_state", "true")].as_slice(),
			"state",
		),
		("initial empty timeline", [("filter", EMPTY_TIMELINE)].as_slice(), "state"),
		(
			"initial full state with empty timeline",
			[("full_state", "true"), ("filter", EMPTY_TIMELINE)].as_slice(),
			"state",
		),
		(
			"incremental full state with empty timeline",
			[("since", since), ("full_state", "true"), ("filter", EMPTY_TIMELINE)].as_slice(),
			"state",
		),
		(
			"quiet stable state after",
			[("since", since), ("full_state", "true"), ("use_state_after", "true")].as_slice(),
			"state_after",
		),
		(
			"quiet unstable state after",
			unstable_query.as_slice(),
			"org.matrix.msc4222.state_after",
		),
	] {
		let response = sync(&client, query).await?;

		for (room, encrypted) in [(&plain, false), (&encrypted, true)] {
			check_room(&response, room, &user, encrypted, state_field)
				.map_err(|error| err!("{case}: {room}: {error}"))?;
		}
	}

	// A nonempty timeline keeps legacy state before its first event. The last
	// topic/encryption event belongs in this one-event timeline, not that state.
	let query = [("full_state", "true"), ("filter", r#"{"room":{"timeline":{"limit":1}}}"#)];

	let nonempty = sync(&client, &query).await?;
	let delta = sync(&client, &[("since", before), ("filter", EMPTY_TIMELINE)]).await?;

	for (room, event_type, content_key, expected) in [
		(&plain, "m.room.topic", "topic", "quiet plain room"),
		(&encrypted, "m.room.encryption", "algorithm", "m.megolm.v1.aes-sha2"),
	] {
		let joined = &delta["rooms"]["join"][room.as_str()];
		let state = joined["state"]["events"]
			.as_array()
			.ok_or_else(|| err!("empty incremental timeline omitted changed state"))?;

		if state_event(state, event_type, "")?["content"][content_key] != expected
			|| joined["timeline"]["events"]
				.as_array()
				.is_none_or(|events| !events.is_empty())
		{
			return Err!("empty incremental timeline lost its final state event");
		}

		let joined = &nonempty["rooms"]["join"][room.as_str()];
		let timeline = joined["timeline"]["events"]
			.as_array()
			.ok_or_else(|| err!("nonempty full-state response omitted the timeline"))?;

		if timeline.len() != 1
			|| state_event(timeline, event_type, "")?["content"][content_key] != expected
		{
			return Err!("nonempty full-state response lost its final state event");
		}

		let state = joined["state"]["events"]
			.as_array()
			.ok_or_else(|| err!("nonempty full-state response omitted legacy state"))?;

		state_event(state, "m.room.create", "")?;
		if state_event(state, "m.room.member", user.as_str())?["content"]["membership"] != "join"
		{
			return Err!(
				"nonempty full-state response lost the syncing user's joined membership"
			);
		}

		if state_event(state, event_type, "").is_ok() {
			return Err!("legacy state included the event at the start of its timeline");
		}
	}

	Ok(())
}

#[tracing::instrument(level = "debug", skip_all)]
async fn sync(client: &Client<'_>, query: &[(&str, &str)]) -> Result<Value> {
	Ok(client
		.services
		.client
		.clients
		.default
		.get(client.url("sync"))
		.bearer_auth(client.token)
		.query(&[("timeout", "0")])
		.query(query)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?)
}

fn check_room(
	response: &Value,
	room: &RoomId,
	user: &UserId,
	encrypted: bool,
	state_field: &str,
) -> Result {
	let joined = &response["rooms"]["join"][room.as_str()];
	let state = joined[state_field]["events"]
		.as_array()
		.ok_or_else(|| err!("quiet room omitted requested state"))?;

	if joined["timeline"]["events"]
		.as_array()
		.is_some_and(|events| !events.is_empty())
	{
		return Err!("quiet room unexpectedly has timeline events");
	}

	if state_field != "state" && joined.get("state").is_some() {
		return Err!("state-after response also included legacy state");
	}

	state_event(state, "m.room.create", "")?;
	let member = state_event(state, "m.room.member", user.as_str())?;

	if member["content"]["membership"] != "join" {
		return Err!("full state lost the syncing user's joined membership");
	}

	if encrypted {
		let encryption = state_event(state, "m.room.encryption", "")?;

		if encryption["content"]["algorithm"] != "m.megolm.v1.aes-sha2"
			|| encryption["unsigned"]["membership"] != "join"
		{
			return Err!("full state lost encryption content or membership annotation");
		}
	} else {
		if state
			.iter()
			.any(|event| event["type"] == "m.room.encryption")
		{
			return Err!("plain room unexpectedly has encryption state");
		}

		if state_event(state, "m.room.topic", "")?["content"]["topic"] != "quiet plain room" {
			return Err!("full state lost the latest topic");
		}
	}

	Ok(())
}

fn state_event<'a>(state: &'a [Value], event_type: &str, key: &str) -> Result<&'a Value> {
	state
		.iter()
		.find(|event| event["type"] == event_type && event["state_key"] == key)
		.ok_or_else(|| err!("full state omitted {event_type} with key {key}"))
}
