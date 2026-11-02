#![cfg(test)]

use serde_json::json;
use tuwunel_core::Result;
use tuwunel_service::Services;

use self::{
	client::{Client, register},
	fixture::boot,
};

mod client;
mod fixture;

#[path = "client/event.rs"]
mod event;

#[path = "client/get.rs"]
mod get;

#[path = "client/put.rs"]
mod put;

#[path = "client/redact.rs"]
mod redact;

#[path = "client/room_event.rs"]
mod room_event;

const TOKEN: &str = "redact-transaction-ids-author-token-0";

// The redaction and the later send share this transaction ID on purpose.
const TXN_ID: &str = "redact";

#[test]
fn redaction_retries_return_the_first_redaction() -> Result {
	boot("redact-transaction-ids", ["logging.enable=false"], exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "author", TOKEN).await?;

	let client = Client { services, base, token: TOKEN };
	let room = client.create_room(&json!({})).await?;
	let target = client
		.send_text(&room, "target", "target")
		.await?;

	let other = client.send_text(&room, "other", "other").await?;
	let latest = async || {
		services
			.timeline
			.latest_pdu_in_room(&room)
			.await
			.map(|pdu| pdu.event_id)
	};

	let first = client.redact(&room, &target, TXN_ID).await?;
	let retry = client.redact(&room, &target, TXN_ID).await?;

	assert_eq!(retry, first, "the retry returned another redaction");
	assert_eq!(latest().await?, first, "the retry appended an event");

	let event = client.event(&room, &first).await?;

	assert_eq!(event["unsigned"]["transaction_id"], TXN_ID, "{event}");

	let retargeted = client.redact(&room, &other, TXN_ID).await?;

	assert_ne!(retargeted, first, "another target reused the redaction");
	assert_eq!(latest().await?, retargeted, "another target appended nothing");

	let again = client.redact(&room, &target, "again").await?;

	assert_ne!(again, first, "a new transaction reused the redaction");
	assert_eq!(latest().await?, again, "a new transaction appended nothing");

	let message = client.send_text(&room, TXN_ID, "message").await?;

	assert_ne!(message, first, "a send reused the redaction's transaction");
	assert_eq!(latest().await?, message, "the send appended nothing");

	let resent = client.send_text(&room, TXN_ID, "message").await?;

	assert_eq!(resent, message, "the send retry returned another event");
	assert_eq!(latest().await?, message, "the send retry appended an event");

	let event = client.event(&room, &message).await?;

	assert_eq!(event["unsigned"]["transaction_id"], TXN_ID, "{event}");

	Ok(())
}
