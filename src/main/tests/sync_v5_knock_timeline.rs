#![cfg(test)]

use serde_json::{Value, json};
use tuwunel_core::{
	Result, implement,
	ruma::{RoomId, events::StateEventType},
};
use tuwunel_service::Services;

use self::{
	client::{Client, field, register},
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

	assert_knock(&zero, &older, older_count);
	assert_knock(&zero, &newer, newer_count);

	let initial = knocker
		.sync(None, &request("timeline", 10))
		.await?;

	assert_knock(&initial, &older, older_count);
	assert_knock(&initial, &newer, newer_count);

	owner.message(&older, "hidden").await?;
	let fresh = knocker.sync(None, &request("fresh", 10)).await?;

	assert_knock(&fresh, &older, older_count);
	assert_knock(&fresh, &newer, newer_count);

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

	assert_knock(&redraw, &older, older_count);
	assert_knock(&redraw, &newer, newer_count);

	let replay = knocker.sync(Some(&quiet), &expanded).await?;

	assert_eq!(replay["rooms"], redraw["rooms"], "replay changed room payloads");
	assert_knock(&replay, &older, older_count);
	assert_knock(&replay, &newer, newer_count);

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
	assert_knock(&redacted, &older, older_count);
	assert_knock(&redacted, &newer, newer_count);

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
	assert_knock(&repeated, &older, repeated_count);

	let reordered = knocker
		.sync(None, &request("reordered", 10))
		.await?;

	assert_knock(&reordered, &older, repeated_count);
	assert_knock(&reordered, &newer, newer_count);

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

#[implement(Client, params = "<'_>")]
async fn sync(&self, previous: Option<&Value>, body: &Value) -> Result<Value> {
	let pos = previous
		.map(|response| field(response, "pos"))
		.transpose()?;

	let url = match pos {
		| None => format!(
			"{}/_matrix/client/unstable/org.matrix.simplified_msc3575/sync?timeout=100",
			self.base
		),
		| Some(pos) => format!(
			"{}/_matrix/client/unstable/org.matrix.simplified_msc3575/sync?timeout=100&pos={pos}",
			self.base
		),
	};

	let response = self
		.post_url(&url, body)
		.await?
		.error_for_status()?
		.json()
		.await?;

	Ok(response)
}

fn assert_knock(response: &Value, room_id: &RoomId, count: u64) {
	let room = &response["rooms"][room_id.as_str()];

	assert!(count > 0);
	assert_eq!(room["membership"], "knock", "knocked room missing: {response}");
	assert_eq!(room["bump_stamp"].as_u64(), Some(count), "wrong knock recency: {room}");
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

#[implement(Client, params = "<'_>")]
async fn put(&self, path: &str, body: &Value) -> Result<Value> {
	let response = self
		.services
		.client
		.clients
		.default
		.put(self.url(path))
		.bearer_auth(self.token)
		.json(body)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	Ok(response)
}
