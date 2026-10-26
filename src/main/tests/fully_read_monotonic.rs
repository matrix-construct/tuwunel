#![cfg(test)]

use futures::TryFutureExt;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tuwunel_core::{
	Result,
	ruma::{
		EventId, OwnedEventId, RoomId, UserId, event_id,
		events::{RoomAccountDataEventType, fully_read::FullyReadEvent},
	},
};
use tuwunel_service::Services;

use self::{
	client::{Client, field, register},
	fixture::boot,
};

mod client;
mod fixture;

const TOKEN: &str = "fully-read-monotonic-reader-token-0";

#[test]
fn fully_read_moves_back_only_on_request() -> Result {
	boot("fully-read-monotonic", ["logging.enable=false"], exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let user = register(services, "reader", TOKEN).await?;
	let client = Client { services, base, token: TOKEN };
	let room = client.create_room(&json!({})).await?;
	let first = message(&client, &room, "first").await?;
	let second = message(&client, &room, "second").await?;
	let third = message(&client, &room, "third").await?;
	let markers = format!("rooms/{room}/read_markers");
	let receipt = |kind: &str, event: &EventId| format!("rooms/{room}/receipt/{kind}/{event}");
	let backward = json!({ "com.beeper.allow_backward": true });
	let marker = || fully_read(services, &user, &room);
	let written = || last_count(services, &user, &room);
	let private = || private_count(services, &user, &room);

	client
		.post(&markers, &json!({ "m.fully_read": first }))
		.await?;

	assert_eq!(marker().await?, first, "first marker");

	client
		.post(&receipt("m.fully_read", &third), &json!({}))
		.await?;

	assert_eq!(marker().await?, third, "forward marker");

	let count = written().await?;
	let behind = json!({
		"m.fully_read": second,
		"m.read": second,
		"m.read.private": second,
	});

	client.post(&markers, &behind).await?;

	assert_eq!(marker().await?, third, "backward read_markers");

	client
		.post(&receipt("m.fully_read", &first), &json!({}))
		.await?;

	assert_eq!(marker().await?, third, "backward receipt");
	assert_eq!(written().await?, count, "backward writes");

	client
		.post(&receipt("m.fully_read", &second), &backward)
		.await?;

	assert_eq!(marker().await?, second, "requested receipt");

	let position = private().await?;
	let behind = json!({
		"m.fully_read": first,
		"m.read.private": first,
		"com.beeper.allow_backward": true,
	});

	client.post(&markers, &behind).await?;

	assert_eq!(marker().await?, first, "requested read_markers");
	assert_eq!(private().await?, position, "private receipt");

	let refused = client
		.post_url(&client.url(&receipt("m.read", &third)), &backward)
		.await?;

	let status = refused.status();
	let body: Value = refused.json().await?;

	assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
	assert_eq!(body["errcode"], "M_INVALID_PARAM", "{body}");

	let unknown = event_id!("$unknown:localhost");

	client
		.post(&markers, &json!({ "m.fully_read": unknown }))
		.await?;

	assert_eq!(marker().await?, unknown, "unresolvable marker");

	let count = written().await?;

	client
		.post(&markers, &json!({ "m.fully_read": unknown }))
		.await?;

	assert_eq!(written().await?, count, "identical marker");

	let versions: Value = services
		.client
		.clients
		.default
		.get(format!("{base}/_matrix/client/versions"))
		.send()
		.await?
		.json()
		.await?;

	assert_eq!(versions["unstable_features"]["com.beeper.msc4446"], true, "{versions}");

	Ok(())
}

async fn message(client: &Client<'_>, room: &RoomId, body: &str) -> Result<OwnedEventId> {
	let url = client.url(&format!("rooms/{room}/send/m.room.message/{body}"));
	let response: Value = client
		.request(Method::PUT, &url, &json!({ "msgtype": "m.text", "body": body }))
		.await?
		.error_for_status()?
		.json()
		.await?;

	Ok(field(&response, "event_id")?.try_into()?)
}

async fn fully_read(services: &Services, user: &UserId, room: &RoomId) -> Result<OwnedEventId> {
	services
		.account_data
		.get_room(room, user, RoomAccountDataEventType::FullyRead)
		.map_ok(|event: FullyReadEvent| event.content.event_id)
		.await
}

async fn last_count(services: &Services, user: &UserId, room: &RoomId) -> Result<u64> {
	services
		.account_data
		.last_count(Some(room), user, None)
		.await
}

async fn private_count(services: &Services, user: &UserId, room: &RoomId) -> Result<u64> {
	services
		.read_receipt
		.private_read_get_count(room, user)
		.map_ok(|(count, _)| count)
		.await
}
