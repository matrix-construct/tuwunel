#![cfg(test)]

use serde_json::{Value, json};
use tuwunel_core::{Result, ruma::EventId};
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

#[expect(dead_code)] // Only the self-membership action is used from the add-on.
#[path = "client/membership.rs"]
mod membership;

#[path = "client/put.rs"]
mod put;

#[path = "client/redact.rs"]
mod redact;

#[path = "client/room_event.rs"]
mod room_event;

const TOKEN: &str = "redact-transaction-ids-author-token-0";
const MEMBER_TOKEN: &str = "redact-transaction-ids-member-token-0";

// The redaction and the later send share this transaction ID on purpose.
const TXN_ID: &str = "redact";

#[test]
fn redaction_retries_return_the_first_redaction() -> Result {
	boot("redact-transaction-ids", ["logging.enable=false"], exercise)
}

#[test]
fn transaction_ids_reach_only_their_sender() -> Result {
	boot("transaction-ids-sender", ["logging.enable=false"], readers)
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

async fn readers(services: &Services, base: &str) -> Result {
	register(services, "author", TOKEN).await?;
	register(services, "member", MEMBER_TOKEN).await?;

	let author = Client { services, base, token: TOKEN };
	let member = Client { services, base, token: MEMBER_TOKEN };
	let room = author
		.create_room(&json!({"preset": "public_chat"}))
		.await?;

	member.act(&room, "join").await?;

	let message = author
		.send_text(&room, "message-txn", "message")
		.await?;

	let target = author
		.send_text(&room, "target-txn", "target")
		.await?;

	let redaction = author
		.redact(&room, &target, "redaction-txn")
		.await?;

	for (event_id, txn) in [(&message, "message-txn"), (&redaction, "redaction-txn")] {
		let context = format!("rooms/{room}/context/{event_id}");

		let event = author.event(&room, event_id).await?;

		assert_eq!(event["unsigned"]["transaction_id"], txn, "{event}");

		let reply = author.get(&context).await?;

		assert_eq!(reply["event"]["unsigned"]["transaction_id"], txn, "{reply}");

		let event = member.event(&room, event_id).await?;

		assert_stripped(&event, &event, event_id);

		let reply = member.get(&context).await?;

		assert_stripped(&reply, &reply["event"], event_id);
	}

	Ok(())
}

/// Asserts the reply serves this event and no transaction ID.
///
/// The whole reply is scanned, so the events served around this one count too.
fn assert_stripped(reply: &Value, served: &Value, event_id: &EventId) {
	assert_eq!(served["event_id"], event_id.as_str(), "{reply}");
	assert!(!reply.to_string().contains("transaction_id"), "{reply}");
}
