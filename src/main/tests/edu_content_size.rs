#![cfg(test)]

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tuwunel_core::{Result, utils::math::Expected};
use tuwunel_service::Services;

use self::{
	client::{Client, register},
	fixture::boot,
};

#[expect(
	dead_code,
	reason = "Only registration and requests are used from the client harness."
)]
mod client;
mod fixture;

#[test]
fn uploaded_keys_are_limited_to_64_kib_each() -> Result {
	boot("edu-content-size", ["logging.enable=false"], exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let token = "uploaded-key-size-test-access-token";
	let user_id = register(services, "owner", token).await?;
	let (_, device_id, _) = services.users.find_from_token(token).await?;
	let client = Client { services, base, token };
	let device_key = |id: &str| {
		json!({
			"user_id": user_id, "device_id": id, "algorithms": [], "keys": {}, "signatures": {}
		})
	};
	let master =
		json!({"user_id": user_id, "usage": ["master"], "keys": {"ed25519:master": "master"}});
	let dehydrated_url =
		client.client_url("unstable/org.matrix.msc3814.v1/", "dehydrated_device");
	let upload = client.url("keys/upload");
	let signing = client.url("keys/device_signing/upload");
	for (size, status) in [(65_536, StatusCode::OK), (65_537, StatusCode::PAYLOAD_TOO_LARGE)] {
		let mut keys = json!({"device_keys": padded(device_key(device_id.as_str()), size)?});
		let id = if status == StatusCode::OK { "KEPT" } else { "NEXT" };
		if status != StatusCode::OK {
			keys["one_time_keys"] = json!({"curve25519:once": "AQID"});
		}
		let dehydrated = json!({
			"device_id": id,
			"device_data": {"algorithm": "org.matrix.msc3814.v2", "device_pickle": "pickle", "nonce": "nonce"},
			"device_keys": padded(device_key(id), size)?,
		});
		for (method, url, body) in [
			(Method::POST, &upload, keys),
			(Method::POST, &signing, json!({"master_key": padded(master.clone(), size)?})),
			(Method::PUT, &dehydrated_url, dehydrated),
		] {
			let response = client.request(method, url, &body).await?;
			assert_eq!(response.status(), status, "{url}: {size} bytes");
			if status != StatusCode::OK {
				assert_eq!(response.json::<Value>().await?["errcode"], "M_TOO_LARGE");
			}
		}
	}
	let dehydrated = services
		.users
		.get_dehydrated_device_id(&user_id)
		.await?;
	assert_eq!(dehydrated.as_str(), "KEPT");
	assert!(
		services
			.users
			.count_one_time_keys(&user_id, &device_id)
			.await
			.values()
			.all(|count| u64::from(*count) == 0)
	);
	Ok(())
}

fn padded(mut key: Value, size: usize) -> Result<Value> {
	key["padding"] = json!("");
	key["padding"] = json!("x".repeat(size.expected_sub(serde_json::to_vec(&key)?.len())));
	Ok(key)
}
