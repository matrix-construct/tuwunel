#![cfg(test)]

use serde_json::{Value, json};
use tuwunel_core::{
	Result, implement, is_equal_to,
	ruma::{RoomId, UserId, events::StateEventType},
};
use tuwunel_database::Json;
use tuwunel_service::Services;

use self::{
	client::{Client, field, register},
	fixture::boot,
	member_event::find_member_event,
};

mod client;
mod fixture;
mod sliding_sync;

#[path = "client/member_event.rs"]
mod member_event;

#[path = "client/put.rs"]
mod put;

#[path = "sliding_sync/resume.rs"]
mod resume;

const OWNER_TOKEN: &str = "sync-v5-knock-timeline-owner-access-token";
const KNOCKER_TOKEN: &str = "sync-v5-knock-timeline-knocker-access-token";

#[test]
fn knocked_room_carries_no_timeline() -> Result { boot("sync-v5-knock-timeline", [], exercise) }

async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "knockowner", OWNER_TOKEN).await?;
	let user = register(services, "knocker", KNOCKER_TOKEN).await?;
	let owner = Client { services, base, token: OWNER_TOKEN };
	let knocker = Client { services, base, token: KNOCKER_TOKEN };
	let knock = json!({ "type": "m.room.join_rules", "content": { "join_rule": "knock" } });
	let body = json!({ "preset": "private_chat", "initial_state": [knock] });
	let older = owner.create_room(&body).await?;
	let newer = owner.create_room(&body).await?;

	knocker
		.post(&format!("knock/{older}"), &json!({}))
		.await?;

	let older_count = services
		.state_cache
		.get_knock_count(&older, &user)
		.await?;

	knocker
		.post(&format!("knock/{newer}"), &json!({}))
		.await?;

	let newer_count = services
		.state_cache
		.get_knock_count(&newer, &user)
		.await?;

	assert!(newer_count > older_count);

	let zero = knocker.sync(None, &request("zero", 0)).await?;

	assert_knock(&zero, &older, &user, older_count);
	assert_knock(&zero, &newer, &user, newer_count);

	let initial = knocker
		.sync(None, &request("timeline", 10))
		.await?;

	assert_knock(&initial, &older, &user, older_count);
	assert_knock(&initial, &newer, &user, newer_count);

	owner.message(&older, "hidden").await?;
	let fresh = knocker.sync(None, &request("fresh", 10)).await?;

	assert_knock(&fresh, &older, &user, older_count);
	assert_knock(&fresh, &newer, &user, newer_count);

	let quiet = knocker
		.sync(Some(&initial), &request("timeline", 10))
		.await?;

	assert!(
		quiet["rooms"][older.as_str()].is_null(),
		"hidden traffic refreshed room: {quiet}"
	);
	assert!(quiet["rooms"][newer.as_str()].is_null(), "quiet room refreshed: {quiet}");
	assert!(field(&quiet, "pos")?.parse::<u64>()? > newer_count);

	let expanded = request("timeline", 20);
	let redraw = knocker.sync(Some(&quiet), &expanded).await?;

	assert_knock(&redraw, &older, &user, older_count);
	assert_knock(&redraw, &newer, &user, newer_count);

	let replay = knocker.sync(Some(&quiet), &expanded).await?;

	assert_eq!(replay["rooms"], redraw["rooms"], "replay changed room payloads");
	assert_knock(&replay, &older, &user, older_count);
	assert_knock(&replay, &newer, &user, newer_count);

	let membership = services
		.state_accessor
		.room_state_get_id(&older, &StateEventType::RoomMember, user.as_str())
		.await?;

	owner
		.put(&format!("rooms/{older}/redact/{membership}/redact-knock"), &json!({}))
		.await?;

	let redacted_count = services
		.state_cache
		.get_knock_count(&older, &user)
		.await?;

	let redacted = knocker
		.sync(Some(&redraw), &request("timeline", 30))
		.await?;

	assert_eq!(redacted_count, older_count, "redaction changed the own knock count");
	assert_knock(&redacted, &older, &user, older_count);
	assert_knock(&redacted, &newer, &user, newer_count);

	owner
		.post(&format!("rooms/{older}/invite"), &json!({ "user_id": user }))
		.await?;

	knocker
		.post(&format!("join/{older}"), &json!({}))
		.await?;

	let message = owner.message(&older, "joined").await?;
	let joined = knocker
		.sync(Some(&redacted), &request("timeline", 30))
		.await?;

	let timeline = joined["rooms"][older.as_str()]["timeline"]
		.as_array()
		.expect("joined timeline");

	assert_eq!(joined["rooms"][older.as_str()]["membership"], "join");
	assert!(
		joined["rooms"][older.as_str()]["bump_stamp"]
			.as_u64()
			.is_some_and(|count| count > newer_count)
	);

	assert!(
		timeline
			.iter()
			.any(|event| event["event_id"] == message["event_id"])
	);

	knocker
		.post(&format!("rooms/{older}/leave"), &json!({}))
		.await?;

	let left = knocker
		.sync(Some(&joined), &request("timeline", 30))
		.await?;

	assert_eq!(left["rooms"][older.as_str()]["membership"], "leave");

	knocker
		.post(&format!("knock/{older}"), &json!({}))
		.await?;

	let repeated_count = services
		.state_cache
		.get_knock_count(&older, &user)
		.await?;

	let repeated = knocker
		.sync(Some(&left), &request("timeline", 30))
		.await?;

	assert!(repeated_count > newer_count);
	assert_knock(&repeated, &older, &user, repeated_count);

	let reordered = knocker
		.sync(None, &request("reordered", 10))
		.await?;

	assert_knock(&reordered, &older, &user, repeated_count);
	assert_knock(&reordered, &newer, &user, newer_count);

	services.db["userroomid_knockedstate"].put((&user, &newer), Json(json!([])));

	let emptied = knocker
		.sync(None, &request("emptied", 10))
		.await?;

	let room = &emptied["rooms"][newer.as_str()];

	assert_eq!(room["membership"], "knock", "emptied knock missing: {emptied}");
	assert!(room.get("invite_state").is_none(), "empty stripped state sent: {room}");

	Ok(())
}

fn request(connection: &str, limit: u64) -> Value {
	json!({
		"conn_id": connection,
		"lists": { "all": {
			"ranges": [[0, 9]], "timeline_limit": limit, "required_state": [["*", "*"]]
		} },
	})
}

fn assert_knock(response: &Value, room_id: &RoomId, user: &UserId, count: u64) {
	let room = &response["rooms"][room_id.as_str()];

	assert!(count > 0);
	assert_eq!(room["membership"], "knock", "knocked room missing: {response}");
	assert_eq!(room["bump_stamp"].as_u64(), Some(count), "wrong knock recency: {room}");
	assert!(room.get("num_live").is_none(), "knocked room counted live events: {room}");

	// Clients classify the room by the first member event about the user.
	let own_membership = find_member_event(&room["invite_state"], user)
		.map(|event| &event["content"]["membership"]);

	assert!(
		own_membership.is_some_and(is_equal_to!("knock")),
		"the user's first stripped member event is not the knock: {room}"
	);

	for field in ["timeline", "required_state"] {
		assert!(
			room.get(field)
				.is_none_or(|events| events.as_array().is_some_and(Vec::is_empty)),
			"knocker received invalid or nonempty {field}: {room}"
		);
	}
}

#[implement(Client, params = "<'_>")]
async fn message(&self, room: &RoomId, transaction: &str) -> Result<Value> {
	self.put(
		&format!("rooms/{room}/send/m.room.message/{transaction}"),
		&json!({ "msgtype": "m.text", "body": transaction }),
	)
	.await
}
