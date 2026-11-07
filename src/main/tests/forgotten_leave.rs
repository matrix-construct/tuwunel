#![cfg(test)]

use std::sync::Arc;

use reqwest::StatusCode;
use serde_json::{Value, json};
use tuwunel_core::{
	Result, err,
	ruma::{RoomId, UserId},
	utils::BoolExt,
};
use tuwunel_service::Services;

use self::{
	client::{Client, accepted, field, register},
	fixture::boot,
};

mod client;
mod fixture;
mod sliding_sync;

#[expect(dead_code)] // Only the raw response helper is used from this add-on.
#[path = "client/get.rs"]
mod get;

#[path = "client/put.rs"]
mod put;

const OWNER_TOKEN: &str = "forgotten-leave-owner-access-token";
const ALICE_TOKEN: &str = "forgotten-leave-alice-a-access-token";
const SECOND_TOKEN: &str = "forgotten-leave-alice-b-access-token";
const MARKER: &[u8] = &[0xCA, 0xF6];

/// A second device receives a forgotten room's departure once.
///
/// Explicit and forced forgetting retain the departure cursor while hiding
/// history, fresh syncs, and the ordinary left-room view.
#[test]
fn forgotten_departure_reaches_other_device() -> Result {
	boot(
		"forgotten-leave-explicit",
		["logging.enable=false", "rooms.forget_forced_upon_leave=false"],
		exercise,
	)?;

	boot(
		"forgotten-leave-forced",
		["logging.enable=false", "rooms.forget_forced_upon_leave=true"],
		exercise,
	)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let owner_id = register(services, "forgotten-owner", OWNER_TOKEN).await?;

	let alice = register(services, "forgotten-alice", ALICE_TOKEN).await?;

	services
		.users
		.create_device(&alice, None, (Some(SECOND_TOKEN), None), None, None, None)
		.await?;

	let owner = Client { services, base, token: OWNER_TOKEN };
	let first = Client { services, base, token: ALICE_TOKEN };
	let second = Client { services, base, token: SECOND_TOKEN };
	let room = owner
		.create_room(&json!({"preset": "private_chat"}))
		.await?;

	owner
		.post(&format!("rooms/{room}/invite"), &json!({"user_id": alice}))
		.await?;

	first
		.post(&format!("rooms/{room}/join"), &json!({}))
		.await?;

	let initial_v3 = sync_v3(&second, None).await?;
	let initial_v5 = second.sliding_sync(None, 0, &list()).await?;

	assert!(
		initial_v3["rooms"]["join"]
			.get(room.as_str())
			.is_some()
	);

	assert_eq!(initial_v5["rooms"][room.as_str()]["membership"], "join");

	first
		.post(&format!("rooms/{room}/leave"), &json!({}))
		.await?;

	let count = services
		.state_cache
		.get_departure_count(&room, &alice)
		.await?;

	let forgotten = services
		.state_cache
		.is_forgotten(&alice, &room)
		.await?;

	assert_eq!(forgotten, services.config.rooms.forget_forced_upon_leave);

	if !services.config.rooms.forget_forced_upon_leave {
		first
			.post(&format!("rooms/{room}/forget"), &json!({}))
			.await?;
	}

	assert_forgotten(services, &alice, &room, count).await?;
	post_forget_data(&owner, &first, &room, &owner_id, &alice).await?;

	let left_v3 = sync_v3(&second, Some(field(&initial_v3, "next_batch")?)).await?;
	let left_v5 = second
		.sliding_sync(Some(field(&initial_v5, "pos")?), 0, &list())
		.await?;

	let legacy = &left_v3["rooms"]["leave"][room.as_str()];
	let sliding = &left_v5["rooms"][room.as_str()];

	assert_own_leave(&legacy["state"]["events"], &alice);
	assert_eq!(legacy["timeline"]["events"], json!([]));
	assert_eq!(sliding["membership"], "leave", "{left_v5}");
	assert_own_leave(&sliding["required_state"], &alice);
	assert!(
		sliding["timeline"]
			.as_array()
			.is_none_or(Vec::is_empty)
	);

	for extension in ["account_data", "typing", "receipts"] {
		assert!(
			left_v5["extensions"][extension]["rooms"]
				.get(room.as_str())
				.is_none(),
			"forgotten room leaked through {extension}: {left_v5}"
		);
	}

	let resumed_v3 = sync_v3(&second, Some(field(&left_v3, "next_batch")?)).await?;
	let resumed_v5 = second
		.sliding_sync(Some(field(&left_v5, "pos")?), 0, &list())
		.await?;

	assert_absent(&resumed_v3, &resumed_v5, &room);

	let fresh_v3 = sync_v3(&second, None).await?;
	let subscription = json!({
		"conn_id": "forgotten-fresh",
		"room_subscriptions": {room.as_str(): {
			"timeline_limit": 10,
			"required_state": [["m.room.member", "$ME"]]
		}}
	});

	let fresh_v5 = second
		.sliding_sync(None, 0, &subscription)
		.await?;

	assert_absent(&fresh_v3, &fresh_v5, &room);

	let history = second
		.get_url(&second.url(&format!("rooms/{room}/messages?dir=b")))
		.await?;

	assert_eq!(history.status(), StatusCode::FORBIDDEN);

	replay_state(services, &room).await?;
	assert_forgotten(services, &alice, &room, count).await?;

	let replayed_v3 = sync_v3(&second, Some(field(&resumed_v3, "next_batch")?)).await?;
	let replayed_v5 = second
		.sliding_sync(Some(field(&resumed_v5, "pos")?), 0, &list())
		.await?;

	assert_absent(&replayed_v3, &replayed_v5, &room);

	owner
		.post(&format!("rooms/{room}/invite"), &json!({"user_id": alice}))
		.await?;

	assert!(
		services
			.state_cache
			.is_forgotten(&alice, &room)
			.await?
			.is_false()
	);

	first
		.post(&format!("rooms/{room}/join"), &json!({}))
		.await?;

	assert!(
		services
			.state_cache
			.is_forgotten(&alice, &room)
			.await?
			.is_false()
	);

	assert!(
		services.db["userroomid_leftstate"]
			.qry(&(&alice, &room))
			.await
			.unwrap_err()
			.is_not_found()
	);

	first
		.post(&format!("rooms/{room}/leave"), &json!({}))
		.await?;

	let next = services
		.state_cache
		.get_departure_count(&room, &alice)
		.await?;

	assert!(next > count, "a new departure reused the old cursor");

	Ok(())
}

async fn post_forget_data(
	owner: &Client<'_>,
	departed: &Client<'_>,
	room: &RoomId,
	owner_id: &UserId,
	user: &UserId,
) -> Result {
	let message = owner
		.put(
			&format!("rooms/{room}/send/m.room.message/after-forget"),
			&json!({"msgtype": "m.text", "body": "after forgetting"}),
		)
		.await?;

	let event = field(&message, "event_id")?;

	owner
		.post(&format!("rooms/{room}/receipt/m.read/{event}"), &json!({}))
		.await?;

	owner
		.put(
			&format!("rooms/{room}/typing/{owner_id}"),
			&json!({"typing": true, "timeout": 30000}),
		)
		.await?;

	departed
		.put(
			&format!("user/{user}/rooms/{room}/account_data/org.tuwunel.test.forgotten"),
			&json!({"secret": true}),
		)
		.await?;

	Ok(())
}

async fn assert_forgotten(
	services: &Services,
	user: &UserId,
	room: &RoomId,
	count: u64,
) -> Result {
	let marker = services.db["userroomid_leftstate"]
		.qry(&(user, room))
		.await?;

	let recorded = services.db["roomuserid_leftcount"]
		.qry(&(room, user))
		.await?;

	assert_eq!(marker.as_ref(), MARKER);
	assert_eq!(recorded.as_ref(), count.to_be_bytes());
	assert!(
		services
			.state_cache
			.is_forgotten(user, room)
			.await?
	);

	assert!(!services.state_cache.is_left(user, room).await);
	assert!(
		services
			.state_cache
			.get_left_count(room, user)
			.await
			.unwrap_err()
			.is_not_found()
	);

	assert_eq!(
		services
			.state_cache
			.get_departure_count(room, user)
			.await?,
		count
	);

	Ok(())
}

fn assert_own_leave(events: &Value, user: &UserId) {
	let events = events
		.as_array()
		.expect("departure omitted its membership state");

	assert_eq!(events.len(), 1, "forgotten departure exposed extra history: {events:?}");
	assert_eq!(events[0]["type"], "m.room.member");
	assert_eq!(events[0]["state_key"], user.as_str());
	assert_eq!(events[0]["content"]["membership"], "leave");
}

fn assert_absent(legacy: &Value, sliding: &Value, room: &RoomId) {
	for membership in ["join", "invite", "leave", "knock"] {
		assert!(
			legacy["rooms"][membership]
				.get(room.as_str())
				.is_none(),
			"{legacy}"
		);
	}

	assert!(sliding["rooms"].get(room.as_str()).is_none(), "{sliding}");
}

async fn sync_v3(client: &Client<'_>, since: Option<&str>) -> Result<Value> {
	let request = client
		.services
		.client
		.clients
		.default
		.get(client.url("sync"))
		.bearer_auth(client.token)
		.query(&[("timeout", "0"), ("filter", r#"{"room":{"include_leave":true}}"#)]);

	let request = since
		.iter()
		.fold(request, |request, since| request.query(&[("since", since)]));

	let response = request.send().await?;

	accepted(response).await
}

fn list() -> Value {
	json!({
		"conn_id": "forgotten-tracked",
		"extensions": {
			"account_data": {"enabled": true},
			"typing": {"enabled": true},
			"receipts": {"enabled": true}
		},
		"lists": {"all": {
			"ranges": [[0, 9]],
			"timeline_limit": 10,
			"required_state": [["m.room.member", "$ME"]]
		}}
	})
}

async fn replay_state(services: &Services, room: &RoomId) -> Result {
	let guard = services.state.mutex.lock(room).await;

	let hash = services
		.state
		.get_room_shortstatehash(room)
		.await?;

	let state = services
		.state_compressor
		.load_shortstatehash_info(hash)
		.await?
		.last()
		.map(|layer| Arc::clone(&layer.full_state))
		.ok_or_else(|| err!("the departure snapshot loaded no layers"))?;

	services
		.state
		.force_state(room, hash, state, Arc::default(), &guard)
		.await
}
