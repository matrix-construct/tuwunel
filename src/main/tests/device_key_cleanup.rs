#![cfg(test)]

use std::{env::temp_dir, fs::remove_dir_all, net::TcpListener};

use futures::future::join;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result,
	ruma::{
		UserId, device_id,
		serde::{Base64, base64::Standard},
	},
	utils::random_string,
};
use tuwunel_service::Services;

use self::client::{Client, register, wait_until_ready};

#[expect(
	dead_code,
	reason = "shared client helpers are used by sibling tests"
)]
mod client;

const ACCESS_TOKEN: &str = "device-key-cleanup-observer-access-token";
const DEVICE_TOKEN: &str = "device-key-cleanup-target-access-token";

#[test]
fn deleted_device_identity_is_not_reused() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let db_path = temp_dir().join(format!("tuwunel-device-key-cleanup-{}", random_string(32)));
	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option(format!("database_path={db_path:?}"))
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
	remove_dir_all(&db_path).ok();
	result
}

async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;
	let user_id = register(services, "keycleanup", ACCESS_TOKEN).await?;
	let device_id = device_id!("REUSED");
	services
		.users
		.create_device(&user_id, Some(device_id), (Some(DEVICE_TOKEN), None), None, None, None)
		.await?;
	let observer = Client { services, base, token: ACCESS_TOKEN };
	let device = Client { services, base, token: DEVICE_TOKEN };
	let original = keys(&user_id, 1);
	upload(&device, &original).await?;
	if query(&observer, &user_id).await?["keys"] != original["keys"] {
		return Err!("Initial device identity was not queryable");
	}

	services
		.users
		.remove_device(&user_id, device_id)
		.await;
	if !query(&observer, &user_id).await?.is_null() {
		return Err!("Deleted device remained queryable");
	}
	let removed = services
		.users
		.get_device_keys(&user_id, device_id)
		.await;
	let identity_removed = removed.is_err_and(|error| error.is_not_found());

	services
		.users
		.create_device(&user_id, Some(device_id), (Some(DEVICE_TOKEN), None), None, None, None)
		.await?;
	let reused = query(&observer, &user_id).await?;
	if !identity_removed || !reused.is_null() {
		return Err!(
			"Deleted identity removed: {identity_removed}; reused device has no keys: {}",
			reused.is_null(),
		);
	}

	let fresh = keys(&user_id, 2);
	upload(&device, &fresh).await?;
	if query(&observer, &user_id).await?["keys"] != fresh["keys"] {
		return Err!("Recreated device did not publish its fresh identity");
	}
	Ok(())
}

async fn upload(client: &Client<'_>, keys: &Value) -> Result {
	client
		.services
		.client
		.clients
		.default
		.post(client.url("keys/upload"))
		.bearer_auth(client.token)
		.json(&json!({"device_keys": keys}))
		.send()
		.await?
		.error_for_status()?;
	Ok(())
}

async fn query(client: &Client<'_>, user_id: &UserId) -> Result<Value> {
	let response: Value = client
		.services
		.client
		.clients
		.default
		.post(client.url("keys/query"))
		.bearer_auth(client.token)
		.json(&json!({"device_keys": {user_id.as_str(): ["REUSED"]}}))
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;
	Ok(response["device_keys"][user_id.as_str()]["REUSED"].clone())
}

fn keys(user_id: &UserId, seed: u8) -> Value {
	let value = Base64::<Standard>::new(vec![seed; 32]).encode();
	json!({
		"user_id": user_id,
		"device_id": "REUSED",
		"algorithms": ["m.olm.v1.curve25519-aes-sha2"],
		"keys": {"curve25519:REUSED": value, "ed25519:REUSED": value},
		"signatures": {},
	})
}
