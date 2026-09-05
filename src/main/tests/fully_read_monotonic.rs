#![cfg(test)]

use std::{
	env::var, fs::remove_dir_all, net::TcpListener, path::PathBuf, process::id as process_id,
	time::Duration,
};

use futures::future::join;
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{Result, err, ruma::UserId};
use tuwunel_service::{Services, users::Register};

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

/// A stale device posting an older `m.fully_read` must not regress the marker
/// that a newer one already advanced.
#[test]
fn fully_read_marker_is_monotonic() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();

	let root = var("TMPDIR").unwrap_or_else(|_| "/nvme/target/tmp".into());
	let db_path = DatabasePath(
		PathBuf::from(root).join(format!("tuwunel-fully-read-monotonic-{}", process_id())),
	);
	let mut args = Args::default_test(&["fresh", "cleanup"]);

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

	let user_id = UserId::parse_with_server_name("fully-read", services.globals.server_name())?;
	let token = "fully-read-monotonic-regression-token";

	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password: Some("fully-read-password"),
			..Default::default()
		})
		.await?;

	services
		.users
		.create_device(&user_id, None, (Some(token), None), None, None, None)
		.await?;

	let room = create_room(services, base, token).await?;
	let first = send_message(services, base, token, &room, "first", "txn-first").await?;
	let second = send_message(services, base, token, &room, "second", "txn-second").await?;

	// A forward write stores.
	read_markers(services, base, token, &room, &second).await?;

	assert_eq!(fully_read_event(services, base, token, &user_id, &room).await?, second);

	// A backwards write is accepted but must not move the marker.
	read_markers(services, base, token, &room, &first).await?;

	assert_eq!(fully_read_event(services, base, token, &user_id, &room).await?, second);

	// The /receipt endpoint must not move it backwards either.
	fully_read_receipt(services, base, token, &room, &first).await?;

	assert_eq!(fully_read_event(services, base, token, &user_id, &room).await?, second);

	Ok(())
}

async fn read_markers(
	services: &Services,
	base: &str,
	token: &str,
	room: &str,
	event: &str,
) -> Result {
	let response = services
		.client
		.clients
		.default
		.post(format!("{base}/_matrix/client/v3/rooms/{room}/read_markers"))
		.bearer_auth(token)
		.json(&json!({
			"m.fully_read": event,
			"m.read": event,
			"m.read.private": event,
		}))
		.send()
		.await?;

	assert_eq!(response.status().as_u16(), 200, "read_markers: {}", response.text().await?);

	Ok(())
}

async fn fully_read_receipt(
	services: &Services,
	base: &str,
	token: &str,
	room: &str,
	event: &str,
) -> Result {
	let response = services
		.client
		.clients
		.default
		.post(format!("{base}/_matrix/client/v3/rooms/{room}/receipt/m.fully_read/{event}"))
		.bearer_auth(token)
		.json(&json!({}))
		.send()
		.await?;

	assert_eq!(response.status().as_u16(), 200, "receipt: {}", response.text().await?);

	Ok(())
}

async fn fully_read_event(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
	room: &str,
) -> Result<String> {
	let response: Value = services
		.client
		.clients
		.default
		.get(format!(
			"{base}/_matrix/client/v3/user/{user_id}/rooms/{room}/account_data/m.fully_read"
		))
		.bearer_auth(token)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	response
		.get("event_id")
		.and_then(Value::as_str)
		.map(str::to_owned)
		.ok_or_else(|| err!("m.fully_read response omitted event_id"))
}

async fn send_message(
	services: &Services,
	base: &str,
	token: &str,
	room: &str,
	body: &str,
	txn: &str,
) -> Result<String> {
	let response: Value = services
		.client
		.clients
		.default
		.put(format!("{base}/_matrix/client/v3/rooms/{room}/send/m.room.message/{txn}"))
		.bearer_auth(token)
		.json(&json!({"msgtype": "m.text", "body": body}))
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	response
		.get("event_id")
		.and_then(Value::as_str)
		.map(str::to_owned)
		.ok_or_else(|| err!("send response omitted event_id"))
}

async fn create_room(services: &Services, base: &str, token: &str) -> Result<String> {
	let response: Value = services
		.client
		.clients
		.default
		.post(format!("{base}/_matrix/client/v3/createRoom"))
		.bearer_auth(token)
		.json(&json!({}))
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	response
		.get("room_id")
		.and_then(Value::as_str)
		.map(str::to_owned)
		.ok_or_else(|| err!("createRoom response omitted room_id"))
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
