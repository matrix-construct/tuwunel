#![cfg(test)]

use serde_json::{Value, json};
use tuwunel_core::Result;
use tuwunel_service::Services;

use self::{
	client::{Client, register},
	fixture::boot,
};

mod client;
mod fixture;

const OWNER_TOKEN: &str = "sync-v5-knock-timeline-owner-access-token";
const KNOCKER_TOKEN: &str = "sync-v5-knock-timeline-knocker-access-token";

#[test]
fn knocked_room_carries_no_timeline() -> Result {
	let options: [&str; 0] = [];

	boot("sync-v5-knock-timeline", options, exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "knockowner", OWNER_TOKEN).await?;
	register(services, "knocker", KNOCKER_TOKEN).await?;

	let owner = Client { services, base, token: OWNER_TOKEN };
	let knocker = Client { services, base, token: KNOCKER_TOKEN };
	let knock = json!({ "type": "m.room.join_rules", "content": { "join_rule": "knock" } });
	let room_id = owner
		.create_room(&json!({ "preset": "private_chat", "initial_state": [knock] }))
		.await?;

	knocker
		.post(&format!("knock/{room_id}"), &json!({}))
		.await?;

	let url = format!("{base}/_matrix/client/unstable/org.matrix.simplified_msc3575/sync");
	let lists = json!({ "lists": { "all": { "ranges": [[0, 9]], "timeline_limit": 10 } } });
	let response: Value = knocker
		.post_url(&url, &lists)
		.await?
		.error_for_status()?
		.json()
		.await?;

	let room = &response["rooms"][room_id.as_str()];
	let timeline = room["timeline"].as_array();

	assert_eq!(room["membership"], "knock", "knocked room missing from the list: {response}");
	assert!(timeline.is_none_or(Vec::is_empty), "knocker reads the timeline: {room}");

	Ok(())
}
