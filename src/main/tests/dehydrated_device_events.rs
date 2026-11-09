#![cfg(test)]

use std::ops::Range;

use reqwest::{Method, Response};
use serde_json::{Value, json};
use tuwunel_core::Result;
use tuwunel_service::Services;

use self::{
	client::{Client, accepted, field, register},
	fixture::boot,
};

#[expect(
	dead_code,
	reason = "Only registration and HTTP requests are used."
)]
mod client;

mod fixture;
#[expect(
	dead_code,
	reason = "Only the raw GET is used from the GET add-on."
)]
#[path = "client/get.rs"]
mod get;

#[path = "client/put.rs"]
mod put;

const OWNER_TOKEN: &str = "dehydrated-device-events-owner-token";
const SENDER_TOKEN: &str = "dehydrated-device-events-sender-token";
const DEVICE_ID: &str = "DEHYDRATED";
const PREFIX: &str = "unstable/org.matrix.msc3814.v1/";

/// Pages a dehydrated device's events over both forms of the events route.
///
/// The GET form carries `next_batch` only while events remain, so an exactly
/// full last page ends the paging and an empty `from` starts it over. The POST
/// form returns a token on every page with events and ends on an empty page.
/// Both treat an empty token as none and refuse another device and an
/// unparsable token.
#[test]
fn pages() -> Result { boot("dehydrated-device-events", ["logging.enable=false"], exercise) }

async fn exercise(services: &Services, base: &str) -> Result {
	let owner = register(services, "dehydratedowner", OWNER_TOKEN).await?;

	register(services, "dehydratedsender", SENDER_TOKEN).await?;

	let client = Client { services, base, token: OWNER_TOKEN };
	let sender = Client { token: SENDER_TOKEN, ..client };
	let device = json!({
		"device_id": DEVICE_ID,
		"device_data": {
			"algorithm": "org.matrix.msc3814.v2",
			"device_pickle": "pickle",
			"nonce": "nonce",
		},
		"device_keys": {
			"user_id": owner.as_str(),
			"device_id": DEVICE_ID,
			"algorithms": [],
			"keys": {},
			"signatures": {},
		},
	});

	let url = client.client_url(PREFIX, "dehydrated_device");
	let response = client.request(Method::PUT, &url, &device).await?;

	accepted(response).await?;
	send(&sender, owner.as_str(), 0..100).await?;

	let first = get_page(&client, None, 0..50, "(A)").await?;
	let first_batch = field(&first, "next_batch")?;
	let exact = get_page(&client, Some(first_batch), 50..100, "(B)").await?;

	assert!(exact.get("next_batch").is_none(), "(B): {exact}");

	send(&sender, owner.as_str(), 100..101).await?;

	let full = get_page(&client, Some(first_batch), 50..100, "(C)").await?;
	let full_batch = field(&full, "next_batch")?;
	let last = get_page(&client, Some(full_batch), 100..101, "(D)").await?;

	assert!(last.get("next_batch").is_none(), "(D): {last}");

	let restarted = get_page(&client, Some(""), 0..50, "(E)").await?;

	assert_eq!(field(&restarted, "next_batch")?, first_batch, "(E): {restarted}");

	let page = post_page(&client, None, 0..50, "(F1)").await?;
	let page = post_page(&client, Some(field(&page, "next_batch")?), 50..100, "(F2)").await?;
	let page = post_page(&client, Some(field(&page, "next_batch")?), 100..101, "(F3)").await?;
	let drained = post_page(&client, Some(field(&page, "next_batch")?), 0..0, "(F4)").await?;

	assert_eq!(drained.get("next_batch"), Some(&Value::Null), "(F4): {drained}");

	post_page(&client, Some(""), 0..50, "(F5)").await?;

	let other = client.client_url(PREFIX, "dehydrated_device/OTHER/events");
	let response = client.get_url(&other).await?;

	refused(response, 403, "M_FORBIDDEN", "(G1)").await?;

	let response = client.post_url(&other, &json!({})).await?;

	refused(response, 403, "M_FORBIDDEN", "(G2)").await?;

	let response = client
		.get_url(&events_url(&client, "?from=garbage"))
		.await?;

	refused(response, 400, "M_INVALID_PARAM", "(H1)").await?;

	let response = client
		.post_url(&events_url(&client, ""), &json!({ "next_batch": "garbage" }))
		.await?;

	refused(response, 400, "M_INVALID_PARAM", "(H2)").await
}

async fn send(sender: &Client<'_>, owner: &str, seqs: Range<u64>) -> Result {
	// One at a time, since the pages assert the order the messages were sent in.
	for seq in seqs {
		let message = json!({ "messages": { owner: { DEVICE_ID: { "seq": seq } } } });

		sender
			.put(&format!("sendToDevice/m.test/{seq}"), &message)
			.await?;
	}

	Ok(())
}

async fn get_page(
	client: &Client<'_>,
	token: Option<&str>,
	expected: Range<u64>,
	round: &str,
) -> Result<Value> {
	let query = token
		.map(|token| format!("?from={token}"))
		.unwrap_or_default();

	let response = client
		.get_url(&events_url(client, &query))
		.await?;

	let page = accepted(response).await?;

	check_seqs(page, expected, round)
}

async fn post_page(
	client: &Client<'_>,
	token: Option<&str>,
	expected: Range<u64>,
	round: &str,
) -> Result<Value> {
	let body = json!({ "next_batch": token });
	let page = client
		.post_json(&events_url(client, ""), &body)
		.await?;

	check_seqs(page, expected, round)
}

async fn refused(response: Response, status: u16, code: &str, round: &str) -> Result {
	assert_eq!(response.status(), status, "{round}");

	let body: Value = response.json().await?;

	assert_eq!(body["errcode"], code, "{round}: {body}");

	Ok(())
}

fn events_url(client: &Client<'_>, query: &str) -> String {
	client.client_url(PREFIX, &format!("dehydrated_device/{DEVICE_ID}/events{query}"))
}

fn check_seqs(page: Value, expected: Range<u64>, round: &str) -> Result<Value> {
	let seqs: Vec<_> = page["events"]
		.as_array()
		.expect("events array")
		.iter()
		.map(|event| event["content"]["seq"].as_u64())
		.collect();

	let expected: Vec<_> = expected.map(Some).collect();

	assert_eq!(seqs, expected, "{round}: {page}");

	Ok(page)
}
