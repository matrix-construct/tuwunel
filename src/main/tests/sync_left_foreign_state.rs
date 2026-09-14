#![cfg(test)]

use std::{fs::remove_dir_all, net::TcpListener, time::Duration};

use futures::StreamExt;
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result, err,
	ruma::{OwnedRoomId, RoomId, UserId, events::AnySyncStateEvent, serde::Raw},
};
use tuwunel_service::{Services, users::Register};

#[test]
fn left_rooms_survive_foreign_leftstate() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();

	let db_path = Args::test_database_path("sync-left-foreign-state");

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

	let user_id = UserId::parse_with_server_name("leftalice", services.globals.server_name())?;
	let token = "sync-left-foreign-leftstate-regression-token";

	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password: Some("sync-left-foreign-state-password"),
			..Default::default()
		})
		.await?;

	services
		.users
		.create_device(&user_id, None, (Some(token), None), None, None, None)
		.await?;

	let object_room = create_room(services, base, token).await?;
	let partial_room = create_room(services, base, token).await?;
	let null_room = create_room(services, base, token).await?;
	let native_room = create_room(services, base, token).await?;

	let rooms = [&object_room, &partial_room, &null_room, &native_room];

	let initial = sync(services, base, token, None).await?;
	let since = next_batch(&initial)?.to_owned();

	drop(initial);

	for room_id in rooms {
		leave_room(services, base, token, room_id).await?;
	}

	let baseline = sync(services, base, token, Some(&since)).await?;

	for room_id in rooms {
		assert_left(&baseline, room_id, "baseline")?;
	}

	drop(baseline);

	let object_state = foreign_leave_pdu(&user_id, &object_room);
	let leftstate = services.db.get("userroomid_leftstate")?;

	leftstate.put_raw((&user_id, &object_room), object_state.to_string());
	leftstate.put_raw((&user_id, &partial_room), partial_leave_event(&user_id).to_string());
	leftstate.put_raw((&user_id, &null_room), "null");

	// Left rooms are scanned by raw byte prefix, which a longer user id extends.
	let neighbor = UserId::parse(format!("{user_id}.example.net"))?;
	let neighbor_room = RoomId::parse("!neighbor:localhost.example.net")?;

	leftstate.put_raw((&neighbor, &neighbor_room), "[]");
	services.clear_cache().await;

	let expected =
		[(&*object_room, 1), (&*partial_room, 0), (&*null_room, 0), (&*native_room, 0)];

	assert_service_readers(services, &user_id, &expected).await?;

	let planted = sync(services, base, token, Some(&since)).await?;

	for room_id in rooms {
		assert_left(&planted, room_id, "after planting foreign left state")?;
	}

	assert_not_left(&planted, &neighbor_room)?;

	Ok(())
}

/// A sibling conduwuit-lineage server writes the leave event itself into
/// `userroomid_leftstate` rather than the state array tuwunel writes, carrying
/// the federation fields a state event does not.
fn foreign_leave_pdu(user_id: &UserId, room_id: &RoomId) -> Value {
	json!({
		"type": "m.room.member",
		"state_key": user_id,
		"sender": user_id,
		"content": { "membership": "leave" },
		"event_id": "$foreignleaveevent:localhost",
		"room_id": room_id,
		"origin_server_ts": 1,
		"depth": 1,
		"prev_events": [],
		"auth_events": [],
		"hashes": { "sha256": "hK7fZLQ1yPYVMhLVBCJyRKGnwDMEQiFCcXBUVJhXOJc" },
		"signatures": {},
	})
}

/// An event object too partial to read as a whole event still must not fail
/// the row.
fn partial_leave_event(user_id: &UserId) -> Value {
	json!({
		"type": "m.room.member",
		"state_key": user_id,
		"sender": user_id,
		"content": { "membership": "leave" },
		"origin_server_ts": 1,
	})
}

/// The service readers of `userroomid_leftstate` must report every left room,
/// lifting a lone leave event into the state array and reading anything else
/// as no cached state.
async fn assert_service_readers(
	services: &Services,
	user_id: &UserId,
	expected: &[(&RoomId, usize)],
) -> Result {
	let rooms_left: Vec<_> = services
		.state_cache
		.rooms_left(user_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let rooms_left_state: Vec<_> = services
		.state_cache
		.rooms_left_state(user_id)
		.collect()
		.await;

	for &(room_id, events) in expected {
		if !rooms_left.iter().any(|left| left == room_id) {
			return Err!("rooms_left omitted {room_id}");
		}

		let state = rooms_left_state
			.iter()
			.find(|(left, _)| left == room_id)
			.map(|(_, state)| state)
			.ok_or_else(|| err!("rooms_left_state omitted {room_id}"))?;

		if state.len() != events {
			return Err!(
				"rooms_left_state returned {} cached events for {room_id}, expected {events}",
				state.len()
			);
		}

		state
			.first()
			.map(|event| assert_lifted_leave(room_id, event))
			.transpose()?;

		// The stripped projection drops event_id and unsigned, so only the count
		// is comparable with the sync-format reader above.
		let stripped = services
			.state_cache
			.left_state(user_id, room_id)
			.await
			.map_err(|e| err!("left_state failed for {room_id}: {e}"))?;

		if stripped.len() != events {
			return Err!(
				"left_state returned {} cached events for {room_id}, expected {events}",
				stripped.len()
			);
		}
	}

	Ok(())
}

/// The origin's federation fields must not survive the lift.
fn assert_lifted_leave(room_id: &RoomId, event: &Raw<AnySyncStateEvent>) -> Result {
	let event: Value = serde_json::from_str(event.json().get())
		.map_err(|e| err!("lifted leave event for {room_id} is not JSON: {e}"))?;

	if event["type"] != "m.room.member" || event["content"]["membership"] != "leave" {
		return Err!("lifted leave event for {room_id} is not a leave member event");
	}

	if event.get("room_id").is_some() || event.get("signatures").is_some() {
		return Err!("lifted leave event for {room_id} kept federation fields");
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

async fn leave_room(services: &Services, base: &str, token: &str, room_id: &RoomId) -> Result {
	services
		.client
		.clients
		.default
		.post(format!("{base}/_matrix/client/v3/rooms/{room_id}/leave"))
		.bearer_auth(token)
		.json(&json!({}))
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
) -> Result<Value> {
	let mut url = format!("{base}/_matrix/client/v3/sync?timeout=0&full_state=true");

	if let Some(since) = since {
		url.push_str("&since=");
		url.push_str(since);
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

fn assert_left(response: &Value, room_id: &RoomId, stage: &str) -> Result {
	response["rooms"]["leave"]
		.get(room_id.as_str())
		.ok_or_else(|| err!("sync omitted left room {room_id} ({stage})"))?;

	Ok(())
}

fn assert_not_left(response: &Value, room_id: &RoomId) -> Result {
	response["rooms"]["leave"]
		.get(room_id.as_str())
		.is_none()
		.then_some(())
		.ok_or_else(|| err!("sync leaked left room {room_id} from a prefix-sharing user"))
}
