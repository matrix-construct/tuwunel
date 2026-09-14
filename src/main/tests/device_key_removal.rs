#![cfg(test)]

use std::{env::temp_dir, fs::remove_dir_all, net::TcpListener};

use futures::future::join;
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde_json::{Value, from_value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result,
	ruma::{
		UserId, device_id,
		serde::{Base64, base64::Standard},
	},
	utils::random_string,
};
use tuwunel_service::{Services, users::Register};

use self::client::{Client, field, wait_until_ready};

#[expect(
	dead_code,
	reason = "the shared client harness exposes helpers used by sibling integration tests"
)]
mod client;

const LOCALPART: &str = "keyholder";
const PASSWORD: &str = "device-key-removal-password";
const ACCESS_TOKEN: &str = "device-key-removal-test-access-token";
const DEVICE: &str = "REMOVEDKEYS";

/// Removing a device removes its identity keys with it.
///
/// A later session under the same device ID must start without the removed
/// device's keys, and a device ID naming one of the user's cross-signing keys
/// must be refused at login rather than overwrite the signing row.
#[test]
fn device_removal_drops_its_identity_keys() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let db_path = temp_dir().join(format!("tuwunel-device-key-removal-{}", random_string(32)));

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

	let user_id = UserId::parse_with_server_name(LOCALPART, services.globals.server_name())?;

	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password: Some(PASSWORD),
			..Default::default()
		})
		.await?;

	services
		.users
		.create_device(
			&user_id,
			Some(device_id!(DEVICE)),
			(Some(ACCESS_TOKEN), None),
			None,
			None,
			None,
		)
		.await?;

	let client = Client { services, base, token: ACCESS_TOKEN };
	let original = device_keys(&user_id, 1);
	let uploaded = upload(&client, &original).await?;

	assert_eq!(uploaded.status(), StatusCode::OK, "first device-key upload");
	assert_eq!(queried_keys(&client, &user_id).await?, Some(original));

	let deleted = delete_device(&client).await?;

	assert_eq!(deleted.status(), StatusCode::OK, "device deletion");

	let session: Value = login(&client, DEVICE)
		.await?
		.error_for_status()?
		.json()
		.await?;

	let token = field(&session, "access_token")?;
	let client = Client { services, base, token };

	assert_eq!(
		queried_keys(&client, &user_id).await?,
		None,
		"a recreated device must not inherit the removed device's keys",
	);

	let replacement = device_keys(&user_id, 2);
	let uploaded = upload(&client, &replacement).await?;

	assert_eq!(uploaded.status(), StatusCode::OK, "replacement device-key upload");
	assert_eq!(queried_keys(&client, &user_id).await?, Some(replacement));

	let public_key = encoded(3, 32);
	let master_key = json!({
		"user_id": user_id,
		"usage": ["master"],
		"keys": {format!("ed25519:{public_key}"): public_key},
	});

	services
		.users
		.add_cross_signing_keys(&user_id, &Some(from_value(master_key)?), &None, &None, true)
		.await?;

	let collision = login(&client, &public_key).await?;

	assert_eq!(
		collision.status(),
		StatusCode::FORBIDDEN,
		"a device ID naming a cross-signing key must be refused",
	);

	Ok(())
}

fn device_keys(user_id: &UserId, seed: u8) -> Value {
	let signing_id = format!("ed25519:{DEVICE}");
	let key = encoded(seed, 32);

	json!({
		"user_id": user_id,
		"device_id": DEVICE,
		"algorithms": [
			"m.olm.v1.curve25519-aes-sha2",
			"m.megolm.v1.aes-sha2",
		],
		"keys": {
			format!("curve25519:{DEVICE}"): key,
			signing_id.clone(): key,
		},
		"signatures": {user_id.as_str(): {signing_id: encoded(seed, 64)}},
	})
}

fn encoded(seed: u8, len: usize) -> String { Base64::<Standard>::new(vec![seed; len]).encode() }

async fn upload(client: &Client<'_>, device_keys: &Value) -> Result<Response> {
	let body = json!({"device_keys": device_keys});

	request(client, Some(client.token), Method::POST, "keys/upload", &body).await
}

async fn queried_keys(client: &Client<'_>, user_id: &UserId) -> Result<Option<Value>> {
	let body = json!({"device_keys": {user_id.as_str(): []}});
	let response: Value = request(client, Some(client.token), Method::POST, "keys/query", &body)
		.await?
		.error_for_status()?
		.json()
		.await?;

	let keys = response["device_keys"][user_id.as_str()]
		.get(DEVICE)
		.cloned();

	Ok(keys)
}

async fn delete_device(client: &Client<'_>) -> Result<Response> {
	let body = json!({
		"auth": {
			"type": "m.login.password",
			"identifier": {"type": "m.id.user", "user": LOCALPART},
			"password": PASSWORD,
		},
	});

	let path = format!("devices/{DEVICE}");

	request(client, Some(client.token), Method::DELETE, &path, &body).await
}

async fn login(client: &Client<'_>, device_id: &str) -> Result<Response> {
	let body = json!({
		"type": "m.login.password",
		"identifier": {"type": "m.id.user", "user": LOCALPART},
		"password": PASSWORD,
		"device_id": device_id,
	});

	request(client, None, Method::POST, "login", &body).await
}

async fn request(
	client: &Client<'_>,
	token: Option<&str>,
	method: Method,
	path: &str,
	body: &Value,
) -> Result<Response> {
	let request = client
		.services
		.client
		.clients
		.default
		.request(method, client.url(path))
		.json(body);

	token
		.into_iter()
		.fold(request, RequestBuilder::bearer_auth)
		.send()
		.await
		.map_err(Into::into)
}
