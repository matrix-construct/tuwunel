#![cfg(test)]

use std::{env::temp_dir, fs::remove_dir_all, net::TcpListener};

use futures::future::join;
use reqwest::{Response, StatusCode};
use serde_json::{Map, Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result,
	ruma::{
		DeviceId, UserId, device_id,
		serde::{Base64, base64::Standard},
	},
	utils::random_string,
};
use tuwunel_service::{Services, users::Register};

use self::client::{Client, wait_until_ready};

#[expect(
	dead_code,
	reason = "the shared client harness exposes helpers used by sibling integration tests"
)]
mod client;

const ACCESS_TOKEN: &str = "device-key-read-errors-test-access-token";
const DEVICE: &str = "READERRORS";

/// Device-key replacement must distinguish an absent row from an unreadable
/// one without disturbing the existing exact-copy retry behavior.
#[test]
fn device_key_read_errors_preserve_stored_bytes() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let db_path =
		temp_dir().join(format!("tuwunel-device-key-read-errors-{}", random_string(32)));

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

	let user_id = UserId::parse_with_server_name("keyreader", services.globals.server_name())?;
	let device_id = device_id!(DEVICE);
	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password: Some("device-key-read-errors-password"),
			..Default::default()
		})
		.await?;

	services
		.users
		.create_device(&user_id, Some(device_id), (Some(ACCESS_TOKEN), None), None, None, None)
		.await?;

	let client = Client { services, base, token: ACCESS_TOKEN };
	let original = device_keys(&user_id, 1, 2, 3);
	let first = upload(&client, &original).await?;
	assert_eq!(first.status(), StatusCode::OK, "first device-key upload");
	assert_eq!(stored_keys(services, &user_id, device_id).await?, original);

	let replacement = device_keys(&user_id, 4, 5, 6);
	let changed = upload(&client, &replacement).await?;
	assert_eq!(changed.status(), StatusCode::OK, "changed device-key upload");
	assert_eq!(stored_keys(services, &user_id, device_id).await?, replacement);

	let exact_with_new_signature = device_keys(&user_id, 4, 5, 7);
	let retry = upload(&client, &exact_with_new_signature).await?;
	assert_eq!(retry.status(), StatusCode::OK, "exact-copy retry");
	assert_eq!(
		stored_keys(services, &user_id, device_id).await?,
		replacement,
		"an exact-key retry must preserve the stored signature",
	);

	let malformed = upload(&client, &json!({})).await?;
	assert_eq!(
		malformed.status(),
		StatusCode::BAD_REQUEST,
		"malformed uploaded keys must remain a client error",
	);
	assert_eq!(
		stored_keys(services, &user_id, device_id).await?,
		replacement,
		"a malformed upload must not change the stored keys",
	);

	let keyid_key = &services.db["keyid_key"];
	let candidate = device_keys(&user_id, 8, 9, 10);
	let mut failures = Vec::new();
	for (case, invalid_bytes) in [
		("missing typed fields", b"{}".as_slice()),
		("invalid JSON syntax", b"not-json".as_slice()),
		("truncated JSON", b"{".as_slice()),
		("invalid UTF-8", b"\x80".as_slice()),
	] {
		keyid_key.put_raw((&user_id, device_id), invalid_bytes);

		let response = upload(&client, &candidate).await?;
		let stored = keyid_key.qry(&(&user_id, device_id)).await?;
		let preserved = stored.as_ref() == invalid_bytes;
		if response.status() != StatusCode::INTERNAL_SERVER_ERROR || !preserved {
			failures.push((case, response.status(), preserved));
		}
	}

	assert!(
		failures.is_empty(),
		"corrupt stored keys must return 500 and remain unchanged: {failures:?}",
	);

	Ok(())
}

async fn upload(client: &Client<'_>, device_keys: &Value) -> Result<Response> {
	Ok(client
		.services
		.client
		.clients
		.default
		.post(client.url("keys/upload"))
		.bearer_auth(client.token)
		.json(&json!({"device_keys": device_keys}))
		.send()
		.await?)
}

async fn stored_keys(
	services: &Services,
	user_id: &UserId,
	device_id: &DeviceId,
) -> Result<Value> {
	let stored = services
		.users
		.get_device_keys(user_id, device_id)
		.await?;

	Ok(serde_json::from_str(stored.json().get())?)
}

fn device_keys(user_id: &UserId, curve_seed: u8, signing_seed: u8, signature_seed: u8) -> Value {
	let curve_id = format!("curve25519:{DEVICE}");
	let signing_id = format!("ed25519:{DEVICE}");

	let mut keys = Map::new();
	keys.insert(curve_id, Value::String(encoded(curve_seed, 32)));
	keys.insert(signing_id.clone(), Value::String(encoded(signing_seed, 32)));

	let mut user_signatures = Map::new();
	user_signatures.insert(signing_id, Value::String(encoded(signature_seed, 64)));

	let mut signatures = Map::new();
	signatures.insert(user_id.to_string(), Value::Object(user_signatures));

	json!({
		"user_id": user_id,
		"device_id": DEVICE,
		"algorithms": [
			"m.olm.v1.curve25519-aes-sha2",
			"m.megolm.v1.aes-sha2",
		],
		"keys": keys,
		"signatures": signatures,
	})
}

fn encoded(seed: u8, len: usize) -> String { Base64::<Standard>::new(vec![seed; len]).encode() }
