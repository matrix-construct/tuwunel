#![cfg(test)]

use std::{
	env::var, fs::remove_dir_all, net::TcpListener, path::PathBuf, process::id as process_id,
	time::Duration,
};

use futures::{StreamExt, pin_mut};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result,
	matrix::pdu::PduBuilder,
	ruma::{
		OwnedEventId, OwnedRoomId, RoomVersionId, UserId, event_id,
		events::room::create::RoomCreateEventContent, room_id,
	},
	utils::stream::ReadyExt,
};
use tuwunel_service::{Services, users::Register};

const OCCURRENCES: usize = 8;

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

#[test]
fn batch_duplicates_share_one_shorteventid() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();

	let root = var("TMPDIR").unwrap_or_else(|_| "/nvme/target/tmp".into());
	let db_path = DatabasePath(
		PathBuf::from(root).join(format!("tuwunel-short-id-allocation-{}", process_id())),
	);

	let mut args = Args::default_test(&["fresh", "cleanup"]);
	args.maintenance = true;
	args.option.extend([
		format!("database_path={:?}", db_path.0),
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

		let outcome = exercise(&services, &base).await;
		let shutdown = server.server.shutdown();

		drop(services);

		let run = async_run(&server).await;
		let stop = async_stop(&server).await;

		outcome.and(shutdown).and(run).and(stop)
	});

	drop(runtime);

	result
}

async fn exercise(services: &Services, base: &str) -> Result {
	create_hash_and_sign_does_not_allocate_short_id(services).await?;
	repeated_identical_state_resend_does_not_allocate_short_id(services, base).await?;

	let event_id = event_id!("$short-id-allocation-batch:localhost");
	// a repeated event misses the batched lookup on every occurrence
	let batch = [event_id; OCCURRENCES];

	let shorts = services
		.short
		.multi_get_or_create_shorteventid(batch.iter().copied());

	pin_mut!(shorts);

	let Some(first) = shorts.next().await else {
		return Err!("batch yielded no short ids");
	};

	if shorts.ready_any(|short| short.ne(&first)).await {
		return Err!("one event id took more than one short id within a batch");
	}

	let resolved: OwnedEventId = services
		.short
		.get_eventid_from_short(first)
		.await?;

	if resolved != event_id {
		return Err!("short id did not resolve back to its event id");
	}

	Ok(())
}

async fn repeated_identical_state_resend_does_not_allocate_short_id(
	services: &Services,
	base: &str,
) -> Result {
	wait_until_ready(services, base).await?;

	let user_id = UserId::parse_with_server_name("shortidalice", services.globals.server_name())?;
	let token = "short-id-allocation-token";

	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password: Some("short-id-allocation-password"),
			..Default::default()
		})
		.await?;

	services
		.users
		.create_device(&user_id, None, (Some(token), None), None, None, None)
		.await?;

	let room_id = create_room(services, base, token).await?;
	let content = json!({"topic": "Short ID resend regression"});

	let first_event_id = send_state_event(services, base, token, &room_id, &content).await?;
	let second_event_id = send_state_event(services, base, token, &room_id, &content).await?;

	if second_event_id != first_event_id.as_str() {
		return Err!("identical state resend returned a different event id");
	}

	let current_event_id = current_state_event_id(services, base, token, &room_id).await?;

	if current_event_id != first_event_id.as_str() {
		return Err!("identical state resend overwrote the room state");
	}

	Ok(())
}

async fn create_hash_and_sign_does_not_allocate_short_id(services: &Services) -> Result {
	let sender = services.globals.server_user.as_ref();
	if !services.users.exists(sender).await {
		services.users.create(sender, None, None).await?;
	}

	let room_id = room_id!("!short-id-no-append:localhost");
	let state_lock = services.state.mutex.lock(room_id).await;
	let (pdu, pdu_json, _prev_state) = services
		.timeline
		.create_hash_and_sign_event(
			PduBuilder::state(String::new(), &RoomCreateEventContent {
				federate: true,
				predecessor: None,
				room_version: RoomVersionId::V11,
				..RoomCreateEventContent::new_v11()
			}),
			sender,
			room_id,
			&state_lock,
		)
		.await?;
	let event_id = pdu.event_id.clone();

	if services
		.short
		.get_shorteventid(&event_id)
		.await
		.is_ok()
	{
		return Err!("create_hash_and_sign_event allocated a short event id before append");
	}

	services
		.timeline
		.append_created_pdu(pdu, pdu_json, sender, &state_lock)
		.await?;

	if services
		.short
		.get_shorteventid(&event_id)
		.await
		.is_err()
	{
		return Err!("append_created_pdu did not allocate a short event id");
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

async fn send_state_event(
	services: &Services,
	base: &str,
	token: &str,
	room_id: &OwnedRoomId,
	content: &Value,
) -> Result<String> {
	let response = services
		.client
		.clients
		.default
		.put(format!(
			"{base}/_matrix/client/v3/rooms/{room_id}/state/m.room.topic/short-id-allocation"
		))
		.bearer_auth(token)
		.json(content)
		.send()
		.await?
		.error_for_status()?
		.json::<Value>()
		.await?;

	response
		.get("event_id")
		.and_then(Value::as_str)
		.map(str::to_owned)
		.ok_or_else(|| err!("state PUT response omitted event_id"))
}

async fn current_state_event_id(
	services: &Services,
	base: &str,
	token: &str,
	room_id: &OwnedRoomId,
) -> Result<String> {
	let response = services
		.client
		.clients
		.default
		.get(format!(
			"{base}/_matrix/client/v3/rooms/{room_id}/state/m.room.topic/short-id-allocation?format=event"
		))
		.bearer_auth(token)
		.send()
		.await?
		.error_for_status()?
		.json::<Value>()
		.await?;

	response
		.get("event_id")
		.and_then(Value::as_str)
		.map(str::to_owned)
		.ok_or_else(|| err!("state GET response omitted event_id"))
}
