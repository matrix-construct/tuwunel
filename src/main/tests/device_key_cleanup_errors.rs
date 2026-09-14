#![cfg(test)]

use std::{net::TcpListener, time::UNIX_EPOCH};

use futures::future::join;
use reqwest::{Response, StatusCode};
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result,
	ruma::{
		DeviceId, UserId, device_id,
		serde::{Base64, base64::Standard},
	},
};
use tuwunel_database::serialize_key;
use tuwunel_service::{Services, users::device::RefreshToken};

use self::client::{Client, register, wait_until_ready};

#[expect(
	dead_code,
	reason = "shared client helpers are used by sibling tests"
)]
mod client;

const PASSWORD: &str = "device-cleanup-error-test-password";
const CORRUPT_KEY: &[u8] = b"not-json";

/// A corrupt row referenced by a signing pointer cannot be safely classified.
/// Deletion and hard refresh expiry must report failure and retain metadata
/// while revoking tokens.
#[test]
fn device_cleanup_decode_errors_fail_http_requests() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={port}"))
		.with_option("refresh_token_hard_logout=true")
		.with_option("listening=true");

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");
		drop(listener);

		let exercise = async {
			let outcome = exercise(&services, &base).await;
			let shutdown = server.server.shutdown();
			outcome.and(shutdown)
		};
		let (run, outcome) = join(async_run(&server), exercise).await;
		drop(services);
		async_stop(&server).await?;
		run?;
		outcome
	})
}

async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;
	let mut failures = Vec::new();
	for batch in [false, true] {
		if let Err(error) = deletion_case(services, base, batch).await {
			failures.push(format!("batch={batch}: {error}"));
		}
	}
	for native in [false, true] {
		if let Err(error) = expired_refresh_case(services, base, native).await {
			failures.push(format!("expired refresh native={native}: {error}"));
		}
	}
	if !failures.is_empty() {
		return Err!("{}", failures.join("; "));
	}
	Ok(())
}

async fn deletion_case(services: &Services, base: &str, batch: bool) -> Result {
	let localpart = if batch { "batchdelete" } else { "singledelete" };
	let observer_token = format!("device-cleanup-error-observer-token-{localpart}");
	let target_token = format!("device-cleanup-error-target-token-{localpart}");
	let next_token = format!("device-cleanup-error-next-token-{localpart}");
	let user_id = register(services, localpart, &observer_token).await?;
	services
		.users
		.set_password(&user_id, Some(PASSWORD))
		.await?;
	let device_id = device_id!("BROKEN");
	services
		.users
		.create_device(&user_id, Some(device_id), (Some(&target_token), None), None, None, None)
		.await?;

	let observer = Client { services, base, token: &observer_token };
	let target = Client { services, base, token: &target_token };
	let next = Client { services, base, token: &next_token };
	let key = (&user_id, device_id);
	let row_key = serialize_key(key)?;
	let keys = &services.db["keyid_key"];
	let metadata = &services.db["userdeviceid_metadata"];
	let original_metadata = metadata.qry(&key).await?.to_vec();
	if batch {
		let next_device = device_id!("NEXT");
		services
			.users
			.create_device(
				&user_id,
				Some(next_device),
				(Some(&next_token), None),
				None,
				None,
				None,
			)
			.await?;
		let next_keys = serde_json::to_vec(&device_keys(&user_id, next_device))?;
		keys.put_raw((&user_id, next_device), &next_keys);
	}

	// This points at the device's actual row. Decoding the row is necessary to
	// distinguish a genuine signing key from a device identity at the same ID.
	services.db["userid_masterkeyid"].insert(&user_id, &row_key);
	keys.put_raw(key, CORRUPT_KEY);
	if whoami(&target).await? != StatusCode::OK {
		return Err!("target device was not authenticated before deletion");
	}

	let response = delete(&observer, &user_id, device_id, batch).await?;
	if response.status() != StatusCode::INTERNAL_SERVER_ERROR {
		return Err!(
			"corrupt pointed row must fail deletion with 500, got {}",
			response.status()
		);
	}
	if keys.qry(&key).await?.as_ref() != CORRUPT_KEY {
		return Err!("failed deletion changed the corrupt identity row");
	}
	if metadata.qry(&key).await?.as_ref() != original_metadata.as_slice() {
		return Err!("failed deletion changed device metadata");
	}
	if whoami(&target).await? != StatusCode::UNAUTHORIZED {
		return Err!("failed key cleanup must still revoke the target device's access token");
	}
	if whoami(&observer).await? != StatusCode::OK {
		return Err!("failed deletion revoked the observing device's access token");
	}
	if batch {
		// A failed item must not cancel the other requested device removals.
		assert_removed(&next, &user_id, device_id!("NEXT")).await?;
	}

	// Repair the payload, retaining the matching pointer. A device identity
	// must then be removable and the same authenticated request can be retried.
	let repaired = serde_json::to_vec(&device_keys(&user_id, device_id))?;
	keys.put_raw(key, &repaired);
	let response = delete(&observer, &user_id, device_id, batch).await?;
	if response.status() != StatusCode::OK {
		return Err!("repaired device deletion must succeed, got {}", response.status());
	}
	assert_removed(&target, &user_id, device_id).await
}

async fn expired_refresh_case(services: &Services, base: &str, native: bool) -> Result {
	if !services.server.config.refresh_token_hard_logout {
		return Err!("expired refresh fixture requires hard logout");
	}
	let localpart = if native { "nativerefresh" } else { "matrixrefresh" };
	let observer_token = format!("device-cleanup-error-observer-token-{localpart}");
	let target_token = format!("device-cleanup-error-target-token-{localpart}");
	let refresh_token = format!("refresh_device_cleanup_error_{localpart}");
	let user_id = register(services, localpart, &observer_token).await?;
	let device_id = device_id!("EXPIRED");
	services
		.users
		.create_device(&user_id, Some(device_id), (Some(&target_token), None), None, None, None)
		.await?;
	services
		.users
		.set_refresh_token(&user_id, device_id, &refresh_token)
		.await?;
	// Keep the real refresh pointers; only move this token's deadline into the past.
	services.db["token_userdeviceid"].raw_put(&refresh_token, (&user_id, device_id, Some(0_u64)));
	if !matches!(
		services.users.classify_refresh_token(&refresh_token).await,
		RefreshToken::Current { user_id: owner, device_id: device, expires_at: Some(expiry) }
			if owner == user_id && device == device_id && expiry == UNIX_EPOCH
	) {
		return Err!("refresh fixture did not classify as the target's expired current token");
	}

	let key = (&user_id, device_id);
	let row_key = serialize_key(key)?;
	let keys = &services.db["keyid_key"];
	let metadata = &services.db["userdeviceid_metadata"];
	let original_metadata = metadata.qry(&key).await?.to_vec();
	services.db["userid_masterkeyid"].insert(&user_id, &row_key);
	keys.put_raw(key, CORRUPT_KEY);
	let target = Client { services, base, token: &target_token };
	let observer = Client { services, base, token: &observer_token };
	if whoami(&target).await? != StatusCode::OK {
		return Err!("target device was not authenticated before refresh expiry");
	}

	let http = &services.client.clients.default;
	let request = if native {
		http.post(format!("{base}/_tuwunel/oidc/token"))
			.form(&[("grant_type", "refresh_token"), ("refresh_token", &refresh_token)])
	} else {
		http.post(target.url("refresh"))
			.json(&json!({"refresh_token": refresh_token}))
	};
	let response = request.send().await?;
	if response.status() != StatusCode::INTERNAL_SERVER_ERROR {
		return Err!(
			"corrupt pointed row must fail expired refresh cleanup with 500, got {}",
			response.status()
		);
	}
	if keys.qry(&key).await?.as_ref() != CORRUPT_KEY
		|| metadata.qry(&key).await?.as_ref() != original_metadata.as_slice()
	{
		return Err!("failed expired refresh cleanup changed the identity or metadata");
	}
	if whoami(&target).await? != StatusCode::UNAUTHORIZED {
		return Err!("failed expired refresh cleanup did not revoke the access token");
	}
	if !matches!(
		services
			.users
			.classify_refresh_token(&refresh_token)
			.await,
		RefreshToken::Unknown
	) || !services.db["token_userdeviceid"]
		.get(&refresh_token)
		.await
		.is_err_and(|error| error.is_not_found())
	{
		return Err!("failed expired refresh cleanup did not revoke the refresh token");
	}
	if whoami(&observer).await? != StatusCode::OK {
		return Err!("failed expired refresh cleanup revoked the observer's access token");
	}
	Ok(())
}

async fn delete(
	client: &Client<'_>,
	user_id: &UserId,
	device_id: &DeviceId,
	batch: bool,
) -> Result<Response> {
	let mut body = json!({
		"auth": {
			"type": "m.login.password",
			"identifier": {"type": "m.id.user", "user": user_id},
			"password": PASSWORD,
		},
	});
	let http = &client.services.client.clients.default;
	let request = if batch {
		body["devices"] = json!([device_id, "NEXT"]);
		http.post(client.url("delete_devices"))
	} else {
		http.delete(client.url(&format!("devices/{device_id}")))
	};
	Ok(request
		.bearer_auth(client.token)
		.json(&body)
		.send()
		.await?)
}

async fn assert_removed(client: &Client<'_>, user_id: &UserId, device_id: &DeviceId) -> Result {
	for column in ["keyid_key", "userdeviceid_metadata"] {
		if !client.services.db[column]
			.qry(&(user_id, device_id))
			.await
			.is_err_and(|error| error.is_not_found())
		{
			return Err!("device {device_id} removal left {column} behind");
		}
	}
	if whoami(client).await? != StatusCode::UNAUTHORIZED {
		return Err!("device {device_id} removal did not revoke its access token");
	}
	Ok(())
}

async fn whoami(client: &Client<'_>) -> Result<StatusCode> {
	Ok(client
		.services
		.client
		.clients
		.default
		.get(client.url("account/whoami"))
		.bearer_auth(client.token)
		.send()
		.await?
		.status())
}

fn device_keys(user_id: &UserId, device_id: &DeviceId) -> Value {
	let value = Base64::<Standard>::new(vec![7; 32]).encode();
	json!({
		"user_id": user_id,
		"device_id": device_id,
		"algorithms": ["m.olm.v1.curve25519-aes-sha2"],
		"keys": {
			format!("curve25519:{device_id}"): value,
			format!("ed25519:{device_id}"): value,
		},
		"signatures": {},
	})
}
