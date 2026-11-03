#![cfg(test)]

use serde_json::{Value, json};
use tuwunel_core::{Err, Result, err, ruma::RoomId};
use tuwunel_service::Services;

use self::{
	client::{Client, field, register},
	fixture::boot,
	membership::room_path,
};

mod client;
mod fixture;

#[path = "client/get.rs"]
mod get;

#[path = "client/membership.rs"]
mod membership;

#[path = "client/put.rs"]
mod put;

const OWNER_TOKEN: &str = "sync-left-foreign-count-owner-access-token";
const LEAVER_TOKEN: &str = "sync-left-foreign-count-leaver-access-token";

const TOPIC_PATH: &str = "state/m.room.topic/";

/// A left room's state stays at the departure when its left count names no event.
///
/// A database migrated from a sibling conduwuit-lineage server records each
/// leave at a count drawn after the leave event rather than at the event. The
/// leaver leaves two rooms, one count is restamped that way, and both topics
/// change afterwards. An initial sync whose timeline reaches each room's
/// creation must serve the topic from before the departure for both.
#[test]
fn foreign_left_count_reads_state_at_departure() -> Result {
	boot("sync-left-foreign-count", [], exercise)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "leftcountowner", OWNER_TOKEN).await?;

	let leaver_id = register(services, "leftcountleaver", LEAVER_TOKEN).await?;

	let client = |token| Client { services, base, token };
	let owner = client(OWNER_TOKEN);
	let leaver = client(LEAVER_TOKEN);

	let room = json!({ "preset": "public_chat", "topic": "before" });
	let foreign = owner.create_room(&room).await?;
	let native = owner.create_room(&room).await?;
	let rooms = [&foreign, &native];

	for room_id in rooms {
		owner.admit(room_id, &leaver, &leaver_id).await?;
		leaver.act(room_id, "leave").await?;
	}

	let stamped = *services.globals.next_count();

	services.db["roomuserid_leftcount"].put_raw((&foreign, &leaver_id), stamped.to_be_bytes());

	for room_id in rooms {
		owner
			.put(&room_path(room_id, TOPIC_PATH), &json!({ "topic": "after" }))
			.await?;
	}

	services.clear_cache().await;

	let filter = json!({ "room": { "include_leave": true, "timeline": { "limit": 50 } } });
	let created = leaver
		.post(&format!("user/{leaver_id}/filter"), &filter)
		.await?;

	let filter_id = field(&created, "filter_id")?;
	let response = leaver
		.get(&format!("sync?timeout=0&filter={filter_id}"))
		.await?;

	for room_id in rooms {
		let topic = left_topic(&response, room_id)?;

		if topic != "before" {
			return Err!("left room {room_id} served topic {topic:?}, not the one at departure");
		}
	}

	Ok(())
}

fn left_topic<'a>(response: &'a Value, room_id: &RoomId) -> Result<&'a str> {
	response["rooms"]["leave"][room_id.as_str()]["state"]["events"]
		.as_array()
		.into_iter()
		.flatten()
		.find(|event| event["type"] == "m.room.topic")
		.and_then(|event| event["content"]["topic"].as_str())
		.ok_or_else(|| err!("sync served no topic for left room {room_id}"))
}
