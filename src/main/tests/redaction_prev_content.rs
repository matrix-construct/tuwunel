#![cfg(test)]

use serde_json::{json, to_value};
use tuwunel_core::Result;
use tuwunel_service::Services;

use self::{
	client::{Client, register},
	fixture::boot,
};

mod client;
mod fixture;

#[expect(dead_code)] // Redaction uses put_event, but this test sends no messages.
#[path = "client/event.rs"]
mod event;
#[path = "client/put.rs"]
mod put;
#[path = "client/redact.rs"]
mod redact;
#[path = "client/state.rs"]
mod state;
#[path = "client/topic.rs"]
mod topic;

#[test]
fn redaction_updates_only_the_successor_prev_content() -> Result {
	boot("redaction-prev-content", [], exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let token = "redaction-prev-content-test-token";
	register(services, "topicowner", token).await?;
	let owner = Client { services, base, token };
	let room = owner.create_room(&json!({})).await?;
	let first = owner.set_topic(&room, "first").await?;
	let second = owner.set_topic(&room, "second").await?;
	let third = owner.set_topic(&room, "third").await?;

	// The first topic is exactly 16 replacements behind the current one.
	for n in 3..=16 {
		owner
			.set_topic(&room, &format!("topic {n}"))
			.await?;
	}

	for txn in ["first-redaction", "repeat-redaction"] {
		owner.redact(&room, &first, txn).await?;

		let second = to_value(services.timeline.get_pdu_json(&second).await?)?;
		let third = to_value(services.timeline.get_pdu_json(&third).await?)?;

		assert_eq!(second["unsigned"]["prev_content"], json!({}));
		assert_eq!(second["unsigned"]["replaces_state"], first.as_str());
		assert_eq!(third["unsigned"]["prev_content"], json!({ "topic": "second" }));
	}

	Ok(())
}
