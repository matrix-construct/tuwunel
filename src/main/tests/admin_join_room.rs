#![cfg(test)]

mod client;

use std::{fs::remove_dir_all, net::TcpListener, path::PathBuf};

use futures::{FutureExt, future::join};
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result,
	ruma::{RoomId, UserId},
};
use tuwunel_service::Services;

use self::client::{Client, register, wait_until_ready};

const ADMIN_TOKEN: &str = "admin-join-room-admin-token-000001";
const TARGET_TOKEN: &str = "admin-join-room-target-token-00001";
const OWNER_TOKEN: &str = "admin-join-room-owner-token-000001";

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) {
		remove_dir_all(&self.0).ok();
	}
}

#[test]
fn admin_join_invites_local_users_to_private_rooms() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let database = DatabasePath(Args::test_database_path("admin-join-room"));
	let args = Args::default_test(&["fresh", "cleanup"])
		.with_database_path(&database.0)
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
			let outcome = exercise(&services, &base).boxed_local().await;
			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (running, outcome) = join(async_run(&server), exercise).await;

		drop(services);
		async_stop(&server).await?;
		running.and(outcome)
	});

	drop(runtime);
	result
}

async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;

	let admin = register(services, "admin-join-admin", ADMIN_TOKEN).await?;
	let target = register(services, "admin-join-target", TARGET_TOKEN).await?;
	let owner = register(services, "admin-join-owner", OWNER_TOKEN).await?;
	services.admin.make_user_admin(&admin).await?;

	let admin_client = Client { services, base, token: ADMIN_TOKEN };
	let owner_client = Client { services, base, token: OWNER_TOKEN };
	let private_room = admin_client
		.create_room(&json!({"preset": "private_chat"}))
		.await?;
	let public_room = admin_client
		.create_room(&json!({"preset": "public_chat"}))
		.await?;
	let foreign_private_room = owner_client
		.create_room(&json!({"preset": "private_chat"}))
		.await?;

	assert_eq!(
		admin_join(&admin_client, &private_room, &target)
			.await?
			.0,
		200
	);
	assert!(
		services
			.state_cache
			.is_joined(&target, &private_room)
			.await
	);

	assert_eq!(
		admin_join(&admin_client, &public_room, &owner)
			.await?
			.0,
		200
	);
	assert!(
		services
			.state_cache
			.is_joined(&owner, &public_room)
			.await
	);

	let (status, response) = admin_join(&admin_client, &foreign_private_room, &target).await?;

	assert_eq!(status, 403, "{response}");
	assert_eq!(response["errcode"], "M_FORBIDDEN");
	Ok(())
}

async fn admin_join(
	client: &Client<'_>,
	room_id: &RoomId,
	user_id: &UserId,
) -> Result<(u16, Value)> {
	let response = client
		.services
		.client
		.clients
		.default
		.post(format!("{}/_synapse/admin/v1/join/{room_id}", client.base))
		.bearer_auth(client.token)
		.json(&json!({"user_id": user_id}))
		.send()
		.await?;

	let status = response.status().as_u16();
	let response = response.json().await?;

	Ok((status, response))
}
