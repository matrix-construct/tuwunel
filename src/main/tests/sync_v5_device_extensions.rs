#![cfg(test)]

use std::time::Duration;

use serde_json::{Value, json};
use tuwunel_core::{
	Result, err,
	utils::{BoolExt, future::ExtExt},
};
use tuwunel_service::Services;

use self::{
	client::{Client, field, register},
	fixture::boot,
};

#[expect(
	dead_code,
	reason = "Only registration and HTTP requests are used."
)]
mod client;
mod fixture;
#[path = "client/put.rs"]
mod put;
mod sliding_sync;

const READER_TOKEN: &str = "sync-v5-device-extensions-reader-token";
const SENDER_TOKEN: &str = "sync-v5-device-extensions-sender-token";
const E2EE_TOKEN: &str = "sync-v5-device-extensions-e2ee-reader-token";

/// Shared server options for the device-extension rounds.
///
/// Presence is off so pings do not move the global count and a caught-up round is really caught up.
const OPTIONS: [&str; 3] = [
	"logging.enable=false",
	"presence.allow_local=false",
	"presence.allow_outgoing=false",
];

/// How long a round with queued messages polls for, in milliseconds.
///
/// The budget sits well above the response deadline so waiting out the poll fails.
const NEWS_POLL_TIMEOUT: u64 = 30_000;

/// Maximum time for a round with queued messages to answer.
///
/// It sits far below the poll budget so a round that waits out its poll fails here.
const NEWS_DEADLINE: Duration = Duration::from_secs(10);

/// Drives to-device pagination, retries, acknowledgements and invalid requests.
///
/// Limited retries prove ordering and prompt redelivery, while a future token
/// omits the section without deleting the final message.
/// Acknowledging the final page on a caught-up request and reopening without
/// `since` proves deletion even when the acknowledgement runs no pass.
/// Invalid limits and unparsable tokens must be refused at the route.
#[test]
fn to_device_extension_pages_and_acknowledges() -> Result {
	boot("sync-v5-device-extensions", OPTIONS, exercise)
}

/// Drives opening, key-upload, device-change and late-enabling e2ee rounds.
///
/// The opening omits device lists but carries both key fields.
/// Uploading a key answers promptly, and a new device answers with unchanged counts.
/// Enabling e2ee on a caught-up connection produces its section immediately.
#[test]
fn e2ee_extension_sections() -> Result {
	boot("sync-v5-e2ee-extension-sections", OPTIONS, exercise_e2ee)
}

async fn exercise_e2ee(services: &Services, base: &str) -> Result {
	let user = register(services, "e2eereader", E2EE_TOKEN).await?;
	let reader = Client { services, base, token: E2EE_TOKEN };
	let body = json!({ "conn_id": "e2ee", "extensions": { "e2ee": { "enabled": true } } });
	let opening = reader.sliding_sync(None, 0, &body).await?;
	let section = &opening["extensions"]["e2ee"];

	assert!(section.is_object(), "(A): {opening}");
	assert!(section.get("device_lists").is_none(), "(A): {opening}");
	assert_eq!(section["device_one_time_keys_count"]["signed_curve25519"], 0, "(A): {opening}");
	assert!(section["device_unused_fallback_key_types"].is_array(), "(A): {opening}");

	let pos = field(&opening, "pos")?;
	let upload = json!({
		"one_time_keys": { "curve25519:AAAAAQ": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" },
	});

	reader.post("keys/upload", &upload).await?;

	let (response, waited) = reader
		.sliding_sync(Some(pos), NEWS_POLL_TIMEOUT, &body)
		.timed()
		.await;

	let uploaded = response?;

	answered_before_deadline(waited, "(B)")?;
	assert_eq!(
		uploaded["extensions"]["e2ee"]["device_one_time_keys_count"]["curve25519"], 1,
		"(B): {uploaded}"
	);

	let pos = field(&uploaded, "pos")?;

	services
		.users
		.create_device(&user, None, (None, None), None, None, None)
		.await?;

	let (response, waited) = reader
		.sliding_sync(Some(pos), NEWS_POLL_TIMEOUT, &body)
		.timed()
		.await;

	let changed = response?;

	answered_before_deadline(waited, "(C)")?;

	let section = &changed["extensions"]["e2ee"];
	let users = section["device_lists"]["changed"]
		.as_array()
		.expect("(C) changed device lists");

	assert!(users.iter().any(|id| id == user.as_str()), "(C): {changed}");
	assert_eq!(section["device_one_time_keys_count"]["curve25519"], 1, "(C): {changed}");

	let late = json!({ "conn_id": "late-e2ee" });
	let opening = reader.sliding_sync(None, 0, &late).await?;
	let pos = field(&opening, "pos")?;
	let enabled = json!({
		"conn_id": "late-e2ee",
		"extensions": { "e2ee": { "enabled": true } },
	});

	let response = reader
		.sliding_sync(Some(pos), 0, &enabled)
		.await?;

	let section = &response["extensions"]["e2ee"];

	assert!(section.is_object(), "(D): {response}");
	assert!(section["device_one_time_keys_count"].is_object(), "(D): {response}");

	Ok(())
}

async fn exercise(services: &Services, base: &str) -> Result {
	let user = register(services, "devicereader", READER_TOKEN).await?;

	register(services, "devicesender", SENDER_TOKEN).await?;

	let reader = Client { services, base, token: READER_TOKEN };
	let sender = Client { token: SENDER_TOKEN, ..reader };
	let body = request(None, 2);
	let opening = reader.sliding_sync(None, 0, &body).await?;
	let pos = field(&opening, "pos")?;

	for seq in 0..3 {
		let message = json!({ "messages": { user.as_str(): { "*": { "seq": seq } } } });

		sender
			.put(&format!("sendToDevice/m.test/{seq}"), &message)
			.await?;
	}

	let first = page(&reader, pos, &body, &[0, 1], "(A)").await?;
	let next_batch = field(&first["extensions"]["to_device"], "next_batch")?;
	let pos = field(&first, "pos")?;

	page(&reader, pos, &body, &[0, 1], "(B)").await?;

	let high = request(Some(&u64::MAX.to_string()), 2);
	let response = reader
		.post_url(&reader.sliding_sync_url(None, 0), &high)
		.await?;

	assert_eq!(response.status(), 200, "(C)");

	let high: Value = response.json().await?;
	assert!(
		high.get("extensions")
			.and_then(|extensions| extensions.get("to_device"))
			.is_none(),
		"(C): {high}"
	);

	let pos = field(&high, "pos")?;
	let acknowledged = request(Some(next_batch), 2);

	let final_page = page(&reader, pos, &acknowledged, &[2], "(D)").await?;
	let next_batch = field(&final_page["extensions"]["to_device"], "next_batch")?;
	let pos = field(&final_page, "pos")?;
	let acknowledged = request(Some(next_batch), 2);
	let response = reader
		.post_url(&reader.sliding_sync_url(Some(pos), 0), &acknowledged)
		.await?;

	assert_eq!(response.status(), 200, "(G)");

	let response = reader
		.post_url(&reader.sliding_sync_url(None, 0), &body)
		.await?;

	assert_eq!(response.status(), 200, "(H)");

	let reopened: Value = response.json().await?;

	assert!(
		reopened
			.get("extensions")
			.and_then(|extensions| extensions.get("to_device"))
			.is_none(),
		"(H): {reopened}"
	);

	let invalid_limit = request(None, 0);
	let invalid_since = request(Some("garbage"), 2);

	refused(&reader, &invalid_limit, "M_BAD_JSON", "(E)").await?;
	refused(&reader, &invalid_since, "M_INVALID_PARAM", "(F)").await?;

	Ok(())
}

fn request(since: Option<&str>, limit: u64) -> Value {
	json!({
		"conn_id": "to-device",
		"extensions": {
			"to_device": { "enabled": true, "limit": limit, "since": since },
		},
	})
}

async fn page(
	reader: &Client<'_>,
	pos: &str,
	body: &Value,
	expected: &[u64],
	round: &str,
) -> Result<Value> {
	let (response, waited) = reader
		.sliding_sync(Some(pos), NEWS_POLL_TIMEOUT, body)
		.timed()
		.await;

	let response = response?;

	answered_before_deadline(waited, round)?;

	let events = response["extensions"]["to_device"]["events"]
		.as_array()
		.expect("to-device events array");

	let sequences: Vec<_> = events
		.iter()
		.map(|event| event["content"]["seq"].as_u64())
		.collect();

	let expected: Vec<_> = expected.iter().copied().map(Some).collect();

	assert_eq!(sequences, expected, "{round}: {response}");

	Ok(response)
}

async fn refused(reader: &Client<'_>, body: &Value, code: &str, round: &str) -> Result {
	let response = reader
		.post_url(&reader.sliding_sync_url(None, 0), body)
		.await?;

	assert_eq!(response.status(), 400, "{round}");

	let body: Value = response.json().await?;

	assert_eq!(body["errcode"], code, "{round}: {body}");

	Ok(())
}

fn answered_before_deadline(waited: Duration, round: &str) -> Result {
	waited
		.lt(&NEWS_DEADLINE)
		.into_option()
		.ok_or_else(|| err!("{round} waited for the poll ({waited:?})"))
}
