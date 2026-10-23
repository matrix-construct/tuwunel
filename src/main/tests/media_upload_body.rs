#![cfg(test)]

use reqwest::{Method, StatusCode};
use serde_json::json;
use tuwunel_core::Result;
use tuwunel_service::Services;

use self::{
	client::{Client, PASSWORD, accepted, field, register},
	fixture::boot,
	upload::media_path,
};

#[expect(dead_code)] // Room helpers are unused by media and UIA requests.
mod client;
mod fixture;

#[expect(dead_code)] // Only the raw GET helper is used.
#[path = "client/get.rs"]
mod get;

#[path = "client/upload.rs"]
mod upload;

const TOKEN: &str = "media-upload-body-test-access-token-valid";

// Both upload routes store object, array and binary bodies without changing a byte.
#[test]
fn media_upload_preserves_body_bytes() -> Result {
	boot(
		"media-upload-body",
		["log_enable=false", "media_upload_min_free_space=0"],
		async |services, base| {
			register(services, "uploader", TOKEN).await?;

			let client = Client { services, base, token: TOKEN };
			let bodies: &[&[u8]] = &[
				b"{\n  \"b\": 1,\n  \"a\": 2\n}\n",
				b"[\n  {\"b\": 1, \"a\": 2}\n]\n",
				b"\x00\xFF\x80\r\nbinary\x00",
			];

			for body in bodies {
				for asynchronous in [false, true] {
					upload_and_download(&client, body, asynchronous).await?;
				}
			}

			Ok(())
		},
	)
}

async fn upload_and_download(
	client: &Client<'_>,
	body: &'static [u8],
	asynchronous: bool,
) -> Result {
	let (method, url, pending) = if asynchronous {
		let response = client
			.post_json(&client.media_url("v1/create"), &json!({}))
			.await?;
		let mxc = field(&response, "content_uri")?.to_owned();
		let path = media_path(&mxc)?;

		(Method::PUT, client.media_url(&format!("v3/upload/{path}")), Some(mxc))
	} else {
		(Method::POST, client.media_url("v3/upload"), None)
	};

	let uploaded = client
		.upload(method, &url, "application/octet-stream", body)
		.await?;
	let uploaded = accepted(uploaded).await?;
	let mxc = match pending.as_deref() {
		| Some(mxc) => mxc,
		| None => field(&uploaded, "content_uri")?,
	};
	let path = media_path(mxc)?;
	let downloaded = client
		.get_url(&client.client_url("v1/", &format!("media/download/{path}")))
		.await?
		.error_for_status()?
		.bytes()
		.await?;

	assert_eq!(downloaded.as_ref(), body, "upload URL: {url}");

	Ok(())
}

// An auth-only UIA retry restores omitted devices; current fields override saved ones.
#[test]
fn uiaa_retry_restores_body_fields() -> Result {
	boot("media-upload-uiaa", ["log_enable=false"], exercise_uiaa)
}

async fn exercise_uiaa(services: &Services, base: &str) -> Result {
	let user = register(services, "uiauser", TOKEN).await?;
	let client = Client { services, base, token: TOKEN };

	for override_devices in [false, true] {
		let target = services
			.users
			.create_device(&user, None, (None, None), None, None, None)
			.await?;
		let response = client
			.post_url(
				&client.url("delete_devices"),
				&json!({
					"devices": [target],
				}),
			)
			.await?;

		assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
		assert!(services.users.device_exists(&user, &target).await);

		let challenge = response.json().await?;
		let session = field(&challenge, "session")?;
		let mut retry = json!({"auth": {
			"type": "m.login.password",
			"identifier": {"type": "m.id.user", "user": user},
			"password": PASSWORD,
			"session": session,
		}});

		if override_devices {
			retry["devices"] = json!([]);
		}

		client.post("delete_devices", &retry).await?;

		assert_eq!(services.users.device_exists(&user, &target).await, override_devices);
	}

	Ok(())
}
