#![cfg(test)]

use std::{fs::remove_dir_all, net::TcpListener, path::PathBuf, time::Duration};

use futures::future::join;
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result, err,
	ruma::{
		MilliSecondsSinceUnixEpoch, OwnedRoomId, RoomId, UserId,
		api::{
			Direction, OutgoingRequest, OutgoingRequestExt,
			federation::{
				authentication::{ServerSignatures, ServerSignaturesInput},
				event::get_event_by_timestamp::v1::Request as TimestampRequest,
			},
			path_builder::SinglePath,
		},
	},
};
use tuwunel_service::{Services, users::Register};

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

#[test]
fn timestamp_route_requires_room_access() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();

	let db_path = DatabasePath(Args::test_database_path("timestamp-access"));
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

	let user_id = UserId::parse_with_server_name("timestamp", services.globals.server_name())?;
	let token = "timestamp-access-regression-token";

	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password: Some("timestamp-access-password"),
			..Default::default()
		})
		.await?;

	services
		.users
		.create_device(&user_id, None, (Some(token), None), None, None, None)
		.await?;

	let room_id = create_room(services, base, token).await?;
	let response = timestamp(services, base, &room_id).await?;

	assert_eq!(response.0, 200, "joined room: {}", response.1);

	leave_room(services, base, token, &room_id).await?;

	let response = timestamp(services, base, &room_id).await?;

	assert_eq!(response.0, 403, "left room: {}", response.1);

	let body: Value = serde_json::from_str(&response.1)?;

	assert_eq!(body["error"], "M_FORBIDDEN: Server is not in room.");

	Ok(())
}

async fn timestamp(services: &Services, base: &str, room_id: &RoomId) -> Result<(u16, String)> {
	let request = TimestampRequest::new(
		room_id.to_owned(),
		MilliSecondsSinceUnixEpoch::now(),
		Direction::Backward,
	);

	send(services, base, request).await
}

async fn send<T>(services: &Services, base: &str, request: T) -> Result<(u16, String)>
where
	T: OutgoingRequest<Authentication = ServerSignatures, PathBuilder = SinglePath>,
{
	let server_name = services.globals.server_name().to_owned();
	let auth = ServerSignaturesInput::new(
		server_name.clone(),
		server_name,
		services.server_keys.keypair(),
	);

	let request = request.try_into_http_request::<Vec<u8>>(base, auth, ())?;
	let response = services
		.client
		.clients
		.default
		.execute(request.try_into()?)
		.await?;

	let status = response.status().as_u16();
	let body = response.text().await?;

	Ok((status, body))
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
