#![cfg(test)]

use std::{fs::remove_dir_all, net::TcpListener, time::Duration};

use futures::future::join;
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result, err,
	pdu::PduBuilder,
	ruma::{OwnedRoomId, UserId, events::room::message::RoomMessageEventContent},
};
use tuwunel_service::{Services, users::Register};

#[test]
fn discarded_build_allocates_no_short_id() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();

	let db_path = Args::test_database_path("short-event-id-allocation");

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

		let (run_result, outcome) = join(async_run(&server), exercise).await;

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

	let user_id = UserId::parse_with_server_name("shortid", services.globals.server_name())?;
	let token = "short-event-id-allocation-token-00000000";

	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password: Some("short-event-id-allocation-password"),
			..Default::default()
		})
		.await?;

	services
		.users
		.create_device(&user_id, None, (Some(token), None), None, None, None)
		.await?;

	let room_id = create_room(services, base, token).await?;
	let state_lock = services.state.mutex.lock(&room_id).await;
	let discarded = PduBuilder::timeline(&RoomMessageEventContent::text_plain("discarded"));

	let (pdu, _) = services
		.timeline
		.create_hash_and_sign_event(discarded, &user_id, &room_id, &state_lock)
		.await?;

	if services
		.short
		.get_shorteventid(&pdu.event_id)
		.await
		.is_ok()
	{
		return Err!("discarded build allocated a short event id");
	}

	let persisted = PduBuilder::timeline(&RoomMessageEventContent::text_plain("persisted"));
	let event_id = services
		.timeline
		.build_and_append_pdu(persisted, &user_id, &room_id, &state_lock)
		.await?;

	if services
		.short
		.get_shorteventid(&event_id)
		.await
		.is_err()
	{
		return Err!("persisted build did not allocate a short event id");
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
