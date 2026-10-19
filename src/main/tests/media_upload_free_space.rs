#![cfg(test)]

use std::net::TcpListener;

use futures::{FutureExt, future::join};
use reqwest::{Response, StatusCode};
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{Err, Result};
use tuwunel_service::Services;

use self::client::{Client, field, register, wait_until_ready};

mod client;

const TOKEN: &str = "media-upload-free-space-access-token";
const FILE: &[u8] = b"a file small enough for any filesystem";

/// More space than any filesystem has left, and the most a TOML integer holds.
const UNREACHABLE: i64 = i64::MAX;

/// Whether the server is booted with a minimum no filesystem can meet.
#[derive(Clone, Copy)]
enum Space {
	Short,
	Sufficient,
}

/// Uploads are refused while storage is short of space, and nothing else is.
///
/// A minimum no filesystem can meet stands in for a full disk, so every one of
/// the three routes an upload begins on is refused with the resource-limit
/// error, while a message is still sent. With the option left at its default
/// the same uploads are stored.
#[test]
fn uploads_are_refused_while_storage_is_short_of_space() -> Result {
	serve(Space::Short)?;
	serve(Space::Sufficient)
}

fn serve(space: Space) -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={port}"))
		.with_option("listening=true")
		// two servers boot in this process and only one may install the logger
		.with_option("log_enable=false");

	let args = match space {
		| Space::Short => args.with_option(format!("media_upload_min_free_space={UNREACHABLE}")),
		| Space::Sufficient => args,
	};

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let driven = async {
			let outcome = exercise(&services, &base, space)
				.boxed_local() // size firewall
				.await;
			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (served, outcome) = join(async_run(&server), driven).await;

		drop(services);
		async_stop(&server).await?;
		served?;

		outcome
	});

	drop(runtime);

	result
}

async fn exercise(services: &Services, base: &str, space: Space) -> Result {
	wait_until_ready(services, base).await?;
	register(services, "uploader", TOKEN).await?;

	match space {
		| Space::Short => uploads_are_refused(services, base).await,
		| Space::Sufficient => uploads_are_stored(services, base).await,
	}
}

/// Each route an upload begins on is refused, and a message is not.
///
/// The asynchronous upload names a media ID nobody reserved, which only a
/// refusal made before the ID is looked up can answer with this error.
async fn uploads_are_refused(services: &Services, base: &str) -> Result {
	let http = &services.client.clients.default;

	let upload = http
		.post(format!("{base}/_matrix/media/v3/upload"))
		.bearer_auth(TOKEN)
		.header("content-type", "text/plain")
		.body(FILE)
		.send()
		.await?;

	expect_refused(upload, "upload").await?;

	let create = http
		.post(format!("{base}/_matrix/media/v1/create"))
		.bearer_auth(TOKEN)
		.json(&json!({}))
		.send()
		.await?;

	expect_refused(create, "create").await?;

	let upload_async = http
		.put(format!("{base}/_matrix/media/v3/upload/localhost/unreserved"))
		.bearer_auth(TOKEN)
		.header("content-type", "text/plain")
		.body(FILE)
		.send()
		.await?;

	expect_refused(upload_async, "asynchronous upload").await?;

	let client = Client { services, base, token: TOKEN };
	let room_id = client.create_room(&json!({})).await?;
	let sent: Value = http
		.put(client.url(&format!("rooms/{room_id}/send/m.room.message/free-space")))
		.bearer_auth(TOKEN)
		.json(&json!({ "msgtype": "m.text", "body": "still talking" }))
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	field(&sent, "event_id").map(|_| ())
}

/// With no minimum set, both ways of uploading store the file.
async fn uploads_are_stored(services: &Services, base: &str) -> Result {
	let http = &services.client.clients.default;

	let uploaded: Value = http
		.post(format!("{base}/_matrix/media/v3/upload"))
		.bearer_auth(TOKEN)
		.header("content-type", "text/plain")
		.body(FILE)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	expect_stored(services, base, field(&uploaded, "content_uri")?).await?;

	let created: Value = http
		.post(format!("{base}/_matrix/media/v1/create"))
		.bearer_auth(TOKEN)
		.json(&json!({}))
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	let content_uri = field(&created, "content_uri")?;

	http.put(format!("{base}/_matrix/media/v3/upload/localhost/{}", media_id(content_uri)?))
		.bearer_auth(TOKEN)
		.header("content-type", "text/plain")
		.body(FILE)
		.send()
		.await?
		.error_for_status()?;

	expect_stored(services, base, content_uri).await
}

/// A refusal names the resource limit, and carries the contact field the
/// specification requires, empty.
async fn expect_refused(response: Response, route: &str) -> Result {
	let status = response.status();
	let body: Value = response.json().await?;

	if status != StatusCode::FORBIDDEN {
		return Err!("{route} answered {status} while short of space: {body}");
	}

	if body.get("errcode").and_then(Value::as_str) != Some("M_RESOURCE_LIMIT_EXCEEDED") {
		return Err!("{route} was refused without the resource-limit error: {body}");
	}

	if body.get("admin_contact").and_then(Value::as_str) != Some("") {
		return Err!("{route} was refused without an empty administrator's contact: {body}");
	}

	Ok(())
}

/// The media a content URI names downloads as the file that was uploaded.
async fn expect_stored(services: &Services, base: &str, content_uri: &str) -> Result {
	let stored = services
		.client
		.clients
		.default
		.get(format!(
			"{base}/_matrix/client/v1/media/download/localhost/{}",
			media_id(content_uri)?
		))
		.bearer_auth(TOKEN)
		.send()
		.await?
		.error_for_status()?
		.bytes()
		.await?;

	if stored != FILE {
		return Err!("{content_uri} did not download as the file uploaded to it");
	}

	Ok(())
}

fn media_id(content_uri: &str) -> Result<&str> {
	match content_uri.rsplit_once('/') {
		| Some((_, media_id)) => Ok(media_id),
		| None => Err!("content_uri names no media id: {content_uri}"),
	}
}
