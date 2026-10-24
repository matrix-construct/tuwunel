#![cfg(test)]

use reqwest::{Method, Response, StatusCode};
use serde_json::{Value, json};
use tuwunel_core::{Err, Result};
use tuwunel_service::Services;

use self::{
	client::{Client, accepted, field, register},
	fixture::boot,
	upload::media_path,
};

mod client;
mod fixture;

#[expect(dead_code)] // Only the raw GET is used from the GET add-on.
#[path = "client/get.rs"]
mod get;

#[path = "client/put.rs"]
mod put;

#[path = "client/upload.rs"]
mod upload;

const TOKEN: &str = "media-upload-free-space-access-token";
const FILE: &[u8] = b"a file small enough for any filesystem";
const FILE_TYPE: &str = "text/plain";

/// Files each upload route must store byte for byte, with their media types.
///
/// A JSON file's whitespace and key order belong to the file, so it is stored
/// as sent whatever its bytes would parse as.
const FILES: [(&[u8], &str); 4] = [
	(FILE, FILE_TYPE),
	(b"{\n  \"b\": 1,\n  \"a\": 2\n}\n", "application/json"),
	(b"[\n  {\"b\": 1, \"a\": 2}\n]\n", "application/json"),
	(b"\x00\xFF\x80\r\nbinary\x00", "application/octet-stream"),
];

// two servers boot in this binary and only one may install the logger
const QUIET: &str = "log_enable=false";

/// Uploads are refused while storage is short of space, and nothing else is.
///
/// A minimum no filesystem can meet, the largest a TOML integer holds, stands
/// in for a full disk, so every route an upload begins on is refused while a
/// message is still sent. With the check off the same routes store every file
/// byte for byte.
#[test]
fn uploads_are_refused_while_storage_is_short_of_space() -> Result {
	let short = [QUIET, &format!("media_upload_min_free_space={}", i64::MAX)];
	let off = [QUIET, "media_upload_min_free_space=0"];

	boot("media-upload-free-space-short", short, uploads_are_refused)?;
	boot("media-upload-free-space-off", off, uploads_are_stored)
}

/// Each route an upload begins on is refused, and a message is not.
///
/// The asynchronous upload names a media ID nobody reserved, so only a refusal
/// made before the ID lookup can answer it with the resource-limit error.
async fn uploads_are_refused(services: &Services, base: &str) -> Result {
	let client = uploader(services, base).await?;
	let upload = client
		.upload(Method::POST, &client.media_url("v3/upload"), FILE_TYPE, FILE)
		.await?;

	expect_refused(upload, "upload").await?;

	let create = client
		.post_url(&client.media_url("v1/create"), &json!({}))
		.await?;

	expect_refused(create, "create").await?;

	let unreserved = client.media_url("v3/upload/localhost/unreserved");
	let upload_async = client
		.upload(Method::PUT, &unreserved, FILE_TYPE, FILE)
		.await?;

	expect_refused(upload_async, "asynchronous upload").await?;

	let room_id = client.create_room(&json!({})).await?;
	let message = json!({ "msgtype": "m.text", "body": "still talking" });
	let sent = client
		.put(&format!("rooms/{room_id}/send/m.room.message/free-space"), &message)
		.await?;

	field(&sent, "event_id").map(|_| ())
}

/// Register the uploading user and return its view of the client API.
///
/// Both servers are exercised as the same user, so each starts from here.
async fn uploader<'a>(services: &'a Services, base: &'a str) -> Result<Client<'a>> {
	register(services, "uploader", TOKEN).await?;

	Ok(Client { services, base, token: TOKEN })
}

/// Expect the resource-limit refusal, with its contact field present and empty.
///
/// The specification requires `admin_contact` on `M_RESOURCE_LIMIT_EXCEEDED`,
/// and clients find a server's contacts through support discovery instead.
async fn expect_refused(response: Response, route: &str) -> Result {
	let status = response.status();
	let body: Value = response.json().await?;
	let errcode = body.get("errcode").and_then(Value::as_str);
	let admin_contact = body.get("admin_contact").and_then(Value::as_str);

	match (status, errcode, admin_contact) {
		| (StatusCode::FORBIDDEN, Some("M_RESOURCE_LIMIT_EXCEEDED"), Some("")) => Ok(()),
		| _ => Err!("{route} was not refused for want of space: {status} {body}"),
	}
}

async fn uploads_are_stored(services: &Services, base: &str) -> Result {
	let client = uploader(services, base).await?;

	for (file, file_type) in FILES {
		file_is_stored(&client, file, file_type).await?;
	}

	Ok(())
}

/// Upload one file on each route an upload begins on and download it back.
///
/// The asynchronous upload fills a content URI reserved for it just before.
async fn file_is_stored(client: &Client<'_>, file: &'static [u8], file_type: &str) -> Result {
	let upload = client
		.upload(Method::POST, &client.media_url("v3/upload"), file_type, file)
		.await?;

	let uploaded = accepted(upload).await?;
	let stored = field(&uploaded, "content_uri")?;

	expect_stored(client, stored, file).await?;

	let created = client
		.post_json(&client.media_url("v1/create"), &json!({}))
		.await?;

	let reserved = field(&created, "content_uri")?;
	let reserved_path = media_path(reserved)?;
	let reserved_url = client.media_url(&format!("v3/upload/{reserved_path}"));
	let upload_async = client
		.upload(Method::PUT, &reserved_url, file_type, file)
		.await?;

	accepted(upload_async).await?;
	expect_stored(client, reserved, file).await
}

async fn expect_stored(client: &Client<'_>, content_uri: &str, file: &[u8]) -> Result {
	let path = media_path(content_uri)?;
	let download = client.client_url("v1/", &format!("media/download/{path}"));
	let stored = client
		.get_url(&download)
		.await?
		.error_for_status()?
		.bytes()
		.await?;

	if stored != file {
		return Err!("{content_uri} downloaded as {stored:?}, not the file uploaded to it");
	}

	Ok(())
}
