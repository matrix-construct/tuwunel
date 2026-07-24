#![cfg(test)]

use std::{
	fs::remove_dir_all, net::TcpListener, path::PathBuf, process::id as process_id,
	time::Duration,
};

use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result, err,
	ruma::{OwnedRoomId, UserId},
};
use tuwunel_service::{Services, users::Register};

const STABLE_REQUEST: &str = "use_state_after";
const STABLE_RESPONSE: &str = "state_after";
const UNSTABLE_REQUEST: &str = "org.matrix.msc4222.use_state_after";
const UNSTABLE_RESPONSE: &str = "org.matrix.msc4222.state_after";

#[test]
fn corrupt_state_after_is_scoped_to_one_joined_room() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();

	let db_path = PathBuf::from("/nvme/target/tmp")
		.join(format!("tuwunel-sync-state-after-corruption-{}", process_id()));
	let mut args = Args::default_test(&["fresh", "cleanup"]);

	args.option.extend([
		format!("database_path={db_path:?}"),
		"address=[\"127.0.0.1\"]".to_owned(),
		format!("port={port}"),
		"listening=true".to_owned(),
	]);

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
		let (run_result, outcome) = tokio::join!(async_run(&server), exercise);

		drop(services);
		async_stop(&server).await?;
		run_result?;

		outcome
	});

	drop(runtime);
	remove_dir_all(&db_path).ok();

	result
}

async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;

	let user_id = UserId::parse_with_server_name("syncalice", services.globals.server_name())?;
	let token = "sync-state-after-corruption-token";

	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password: Some("sync-state-after-test-password"),
			..Default::default()
		})
		.await?;

	services
		.users
		.create_device(&user_id, None, (Some(token), None), None, None, None)
		.await?;

	let healthy_room = create_room(services, base, token).await?;
	let corrupt_room = create_room(services, base, token).await?;
	let initial = sync(services, base, token, None, false, None).await?;
	let since = next_batch(&initial)?.to_owned();

	drop(initial);

	let legacy_shortstatehash = services
		.state
		.get_room_shortstatehash(&corrupt_room)
		.await?;

	send_message(services, base, token, &healthy_room, "healthy").await?;
	send_topic(services, base, token, &corrupt_room).await?;

	let after_shortstatehash = services
		.state
		.get_room_shortstatehash(&corrupt_room)
		.await?;

	if legacy_shortstatehash == after_shortstatehash {
		return Err!("topic event did not advance room state");
	}

	let stable = sync(services, base, token, Some(&since), true, Some(STABLE_REQUEST)).await?;

	assert_requested_field(&stable, [&healthy_room, &corrupt_room], STABLE_RESPONSE)?;
	drop(stable);

	let unstable =
		sync(services, base, token, Some(&since), true, Some(UNSTABLE_REQUEST)).await?;

	assert_requested_field(&unstable, [&healthy_room, &corrupt_room], UNSTABLE_RESPONSE)?;

	let empty_since = next_batch(&unstable)?.to_owned();

	drop(unstable);

	send_message(services, base, token, &healthy_room, "stable-empty").await?;

	let stable_empty =
		sync(services, base, token, Some(&empty_since), false, Some(STABLE_REQUEST)).await?;

	assert_empty_requested_field(&stable_empty, &healthy_room, STABLE_RESPONSE)?;

	let empty_since = next_batch(&stable_empty)?.to_owned();

	drop(stable_empty);
	send_message(services, base, token, &healthy_room, "unstable-empty").await?;

	let unstable_empty =
		sync(services, base, token, Some(&empty_since), false, Some(UNSTABLE_REQUEST)).await?;

	assert_empty_requested_field(&unstable_empty, &healthy_room, UNSTABLE_RESPONSE)?;
	drop(unstable_empty);

	let statediffs = services.db.get("shortstatehash_statediff")?;

	statediffs.remove(&after_shortstatehash.to_be_bytes());
	services.clear_cache().await;

	let fallback = sync(services, base, token, Some(&since), true, Some(STABLE_REQUEST)).await?;

	next_batch(&fallback)?;

	let healthy = joined_room(&fallback, &healthy_room)?;

	if healthy.get(STABLE_RESPONSE).is_none() {
		return Err!("healthy room omitted stable state-after");
	}

	let corrupt = joined_room(&fallback, &corrupt_room)?;

	let legacy_events = corrupt
		.get("state")
		.and_then(|state| state.get("events"))
		.and_then(Value::as_array)
		.ok_or_else(|| err!("corrupt room omitted legacy state events"))?;

	if legacy_events.is_empty() || corrupt.get(STABLE_RESPONSE).is_some() {
		return Err!("corrupt room did not fall back to legacy state");
	}

	drop(fallback);

	statediffs.remove(&legacy_shortstatehash.to_be_bytes());
	services.clear_cache().await;

	let omitted = sync(services, base, token, Some(&since), true, Some(STABLE_REQUEST)).await?;

	next_batch(&omitted)?;

	if joined_room(&omitted, &healthy_room)?
		.get(STABLE_RESPONSE)
		.is_none()
	{
		return Err!("healthy room was lost with corrupt room");
	}

	if omitted["rooms"]["join"]
		.get(corrupt_room.as_str())
		.is_some()
	{
		return Err!("room with two corrupt state boundaries was not omitted");
	}

	Ok(())
}

async fn wait_until_ready(services: &Services, base: &str) -> Result {
	let url = format!("{base}/_matrix/client/versions");

	timeout(Duration::from_secs(10), async {
		loop {
			if services
				.client
				.clients
				.default
				.get(&url)
				.send()
				.await
				.is_ok()
			{
				break;
			}

			sleep(Duration::from_millis(20)).await;
		}
	})
	.await
	.map_err(|_| err!("server listener did not become ready"))?;

	Ok(())
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

async fn send_message(
	services: &Services,
	base: &str,
	token: &str,
	room_id: &OwnedRoomId,
	transaction_id: &str,
) -> Result {
	services
		.client
		.clients
		.default
		.put(format!(
			"{base}/_matrix/client/v3/rooms/{room_id}/send/m.room.message/{transaction_id}"
		))
		.bearer_auth(token)
		.json(&json!({"msgtype": "m.text", "body": "healthy"}))
		.send()
		.await?
		.error_for_status()?;

	Ok(())
}

async fn send_topic(
	services: &Services,
	base: &str,
	token: &str,
	room_id: &OwnedRoomId,
) -> Result {
	services
		.client
		.clients
		.default
		.put(format!("{base}/_matrix/client/v3/rooms/{room_id}/state/m.room.topic/test"))
		.bearer_auth(token)
		.json(&json!({"topic": "after"}))
		.send()
		.await?
		.error_for_status()?;

	Ok(())
}

async fn sync(
	services: &Services,
	base: &str,
	token: &str,
	since: Option<&str>,
	full_state: bool,
	state_after: Option<&str>,
) -> Result<Value> {
	let mut url = format!("{base}/_matrix/client/v3/sync?timeout=0");

	if let Some(since) = since {
		url.push_str("&since=");
		url.push_str(since);
	}

	if full_state {
		url.push_str("&full_state=true");
	}

	if let Some(state_after) = state_after {
		url.push('&');
		url.push_str(state_after);
		url.push_str("=true");
	}

	services
		.client
		.clients
		.default
		.get(url)
		.bearer_auth(token)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await
		.map_err(Into::into)
}

fn next_batch(response: &Value) -> Result<&str> {
	response
		.get("next_batch")
		.and_then(Value::as_str)
		.ok_or_else(|| err!("sync response omitted next_batch"))
}

fn assert_requested_field<const N: usize>(
	response: &Value,
	room_ids: [&OwnedRoomId; N],
	field: &str,
) -> Result {
	for room_id in room_ids {
		let room = joined_room(response, room_id)?;

		if room.get(field).is_none() || room.get("state").is_some() {
			return Err!("room {room_id} did not use requested state field {field}");
		}
	}

	Ok(())
}

fn assert_empty_requested_field(response: &Value, room_id: &OwnedRoomId, field: &str) -> Result {
	let room = joined_room(response, room_id)?;
	let state = room
		.get(field)
		.ok_or_else(|| err!("room {room_id} omitted requested state field {field}: {room}"))?;
	let empty = state
		.as_object()
		.is_some_and(serde_json::Map::is_empty)
		|| state
			.get("events")
			.and_then(Value::as_array)
			.is_some_and(Vec::is_empty);

	if !empty || room.get("state").is_some() {
		return Err!("room {room_id} did not return empty requested field {field}");
	}

	Ok(())
}

fn joined_room<'a>(response: &'a Value, room_id: &OwnedRoomId) -> Result<&'a Value> {
	response["rooms"]["join"]
		.get(room_id.as_str())
		.ok_or_else(|| err!("sync response omitted joined room {room_id}"))
}
