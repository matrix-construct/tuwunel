#![cfg(test)]

use futures::StreamExt;
use reqwest::StatusCode;
use serde_json::json;
use tuwunel_core::{
	Result,
	ruma::{OwnedDeviceId, UserId, api::error::ErrorKind},
};
use tuwunel_service::Services;

use self::{
	client::{Client, register},
	fixture::boot,
};

#[expect(
	dead_code,
	reason = "Only registration and request helpers are shared with the client API harness."
)]
mod client;

mod fixture;

const TOKEN: &str = "edu-size-limits-test-access-token";

const MAS_SECRET: &str = "edu-size-limits-test-mas-secret";

/// Payloads that other servers receive inside an EDU stay within Synapse's
/// limits.
///
/// A to-device message's content is capped at 64 KiB and device IDs, chosen or
/// addressed, at 512 bytes, so no local user can make an EDU that a peer
/// refuses and that would then ride along with every later transaction.
#[test]
fn edu_size_limits() -> Result {
	let options = [
		"allow_registration=true",
		"yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse=true",
		&format!("mas_secret=\"{MAS_SECRET}\""),
	];

	boot("edu-size-limits", options, exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let user_id = register(services, "sender", TOKEN).await?;
	let client = Client { services, base, token: TOKEN };

	let (ok, refused) = (StatusCode::OK, StatusCode::PAYLOAD_TOO_LARGE);
	let all = "*";
	let fits = "d".repeat(512);
	let too_long = "d".repeat(513);

	assert_eq!(send_to_device(&client, &user_id, all, 65_536).await?, ok);
	assert_eq!(send_to_device(&client, &user_id, all, 65_537).await?, refused);
	assert_eq!(send_to_device(&client, &user_id, &fits, 16).await?, ok);
	assert_eq!(send_to_device(&client, &user_id, &too_long, 17).await?, refused);

	let create = async |len: usize| {
		let device_id = OwnedDeviceId::from("d".repeat(len));

		services
			.users
			.create_device(&user_id, Some(&device_id), (None, None), None, None, None)
			.await
	};

	create(512).await?;

	let error = create(513)
		.await
		.expect_err("a 513-byte device ID is refused");

	assert_eq!(error.kind(), ErrorKind::InvalidParam);

	assert_registration_keeps_name(services, base).await?;
	assert_sync_keeps_devices(services, base, &user_id).await
}

/// Send the user's `device` one to-device message whose content is `len` bytes.
///
/// The content pads a ten-byte `{"pad":""}` wrapper, so a `len` below ten sends
/// the ten-byte minimum. `len` also names the transaction, so each call needs
/// its own.
async fn send_to_device(
	client: &Client<'_>,
	user_id: &UserId,
	device: &str,
	len: usize,
) -> Result<StatusCode> {
	let content = json!({ "pad": "x".repeat(len.saturating_sub(10)) });
	let url = client.url(&format!("sendToDevice/m.test/{len}"));
	let status = client
		.services
		.client
		.clients
		.default
		.put(url)
		.bearer_auth(client.token)
		.json(&json!({ "messages": { user_id: { device: content } } }))
		.send()
		.await?
		.status();

	Ok(status)
}

/// A registration refused for its device ID leaves the username free.
///
/// The ID is checked before the account is created, so a retry with a valid
/// ID registers the same name.
async fn assert_registration_keeps_name(services: &Services, base: &str) -> Result {
	let client = Client { services, base, token: "" };
	let url = client.url("register");
	let attempt = async |len: usize| {
		let body = json!({"username": "late", "password": "late-password",
			"device_id": "d".repeat(len), "auth": {"type": "m.login.dummy"}});

		client.post_url(&url, &body).await
	};

	assert_eq!(attempt(513).await?.status(), StatusCode::BAD_REQUEST);
	assert_eq!(attempt(512).await?.status(), StatusCode::OK);

	Ok(())
}

/// A MAS device sync refused for one addition changes no device.
///
/// Every addition is checked before any removal, so the user keeps its devices
/// and gains none of those asked for.
async fn assert_sync_keeps_devices(services: &Services, base: &str, user_id: &UserId) -> Result {
	let mas = Client { services, base, token: MAS_SECRET };
	let url = format!("{base}/_synapse/mas/sync_devices");
	let body = json!({"localpart": user_id.localpart(), "devices": ["NEW", "d".repeat(513)]});
	let devices = async || -> Vec<OwnedDeviceId> {
		services
			.users
			.all_device_ids(user_id)
			.map(ToOwned::to_owned)
			.collect()
			.await
	};

	let before = devices().await;
	let status = mas.post_url(&url, &body).await?.status();

	assert_eq!(status, StatusCode::BAD_REQUEST);
	assert_eq!(devices().await, before);

	Ok(())
}
