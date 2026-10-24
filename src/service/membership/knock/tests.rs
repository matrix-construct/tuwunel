use ruma::{
	RoomId, RoomVersionId, UserId, api::federation::membership::RawStrippedState, event_id,
	room_id, user_id,
};
use serde_json::{Value, from_str, from_value, json, value::to_raw_value};
use tuwunel_core::{Result, config::Figment};
use tuwunel_matrix::PduEvent;

use crate::{
	Services,
	test_utils::{
		json_events, keyed_fixture, pdu_json, signed_state, with_field, with_state_key,
	},
};

/// A remote knock records only the room's verified full PDUs and its legacy
/// hints, ending with the user's own knock.
///
/// No summary event becomes an outlier or room state, and a membership the
/// summary claims for a local user is not recorded.
#[tokio::test]
async fn a_remote_knock_records_only_its_verified_summary() -> Result {
	let Some(fixture) = keyed_fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!knock:localhost");
	let alice = user_id!("@alice:localhost");
	let carol = user_id!("@carol:localhost");
	let event = |kind, state_key, content: Value| {
		signed_state(services, room_id, kind, state_key, &content)
	};

	let (name_id, name) = event("m.room.name", "", json!({"name": "Knock"}))?;
	let (topic_id, topic) = event("m.room.topic", "", json!({"topic": "genuine"}))?;
	let (avatar_id, avatar) = event("m.room.avatar", "", json!({"url": "mxc://localhost/a"}))?;
	let (member_id, member) =
		event("m.room.member", carol.as_str(), json!({"membership": "join"}))?;

	let (foreign_id, foreign) = signed_state(
		services,
		room_id!("!other:localhost"),
		"m.room.history_visibility",
		"",
		&json!({"history_visibility": "shared"}),
	)?;

	let key_id = services.server_keys.active_key_id();
	let signatures = json!({"localhost": {key_id.as_str(): "AAAA"}});
	let altered = with_field(topic, "content", json!({"topic": "altered"}));
	let forged = with_field(avatar, "signatures", signatures);

	let unsigned = json!({
		"room_id": room_id, "sender": "@bob:localhost", "type": "m.room.canonical_alias",
		"state_key": "", "content": {"alias": "#forged:localhost"}, "origin_server_ts": 1,
		"depth": 1, "prev_events": [], "auth_events": [], "hashes": {"sha256": "unchecked"},
		"signatures": {},
	});

	let summary = vec![
		legacy("m.room.create", "", &json!({"room_version": "11"}))?,
		wire(&name)?,
		wire(&altered)?,
		wire(&forged)?,
		wire(&foreign)?,
		wire(&member)?,
		wire(&unsigned)?,
		legacy("m.room.join_rules", "", &json!({"join_rule": "knock"}))?,
		legacy("m.room.member", alice.as_str(), &json!({"membership": "join"}))?,
	];

	let membership = &services.membership;
	let knock = knock(room_id, alice)?;
	let summary = membership
		.knock_summary(room_id, &RoomVersionId::V11, &knock, summary)
		.await?;

	membership
		.record_knock(room_id, alice, &knock, summary)
		.await?;

	let shown = shown(services, alice, room_id).await?;

	assert_eq!(cells(&shown), [
		("m.room.create", ""),
		("m.room.name", ""),
		("m.room.member", carol.as_str()),
		("m.room.join_rules", ""),
		("m.room.member", alice.as_str()),
	]);

	assert_eq!(shown[4]["content"]["membership"], "knock", "the user's own entry is the knock");

	for event_id in [&name_id, &topic_id, &avatar_id, &member_id, &foreign_id] {
		assert!(!services.timeline.pdu_exists(event_id).await, "a summary event is no outlier");
	}

	let room_state = services
		.state
		.get_room_shortstatehash(room_id)
		.await;

	let knocked = services
		.state_cache
		.is_knocked(alice, room_id)
		.await;

	let joined = services
		.state_cache
		.is_joined(carol, room_id)
		.await;

	assert!(room_state.is_err(), "the summary is no room state");
	assert!(knocked);
	assert!(!joined, "the summary sets no local membership");

	Ok(())
}

/// A remote knock clears the user's room and thread unread counts.
///
/// Both notification and highlight counts reset without appending an event.
#[tokio::test]
async fn a_remote_knock_clears_unread_counts() -> Result {
	let Some(fixture) = keyed_fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!knock-unread:localhost");
	let alice = user_id!("@alice:localhost");
	let root = event_id!("$thread");

	for map in ["userroomid_notificationcount", "userroomid_highlightcount"] {
		services.db[map].put((alice, room_id), 3_u64);
		services.db[map].put((alice, room_id, root), 2_u64);
	}

	let membership = &services.membership;
	let knock = knock(room_id, alice)?;
	let summary = membership
		.knock_summary(room_id, &RoomVersionId::V11, &knock, Vec::new())
		.await?;

	membership
		.record_knock(room_id, alice, &knock, summary)
		.await?;

	let pusher = &services.pusher;
	let notifications = pusher.notification_count(alice, room_id).await;
	let highlights = pusher.highlight_count(alice, room_id).await;
	let threads = pusher
		.thread_notification_counts(alice, room_id)
		.await;

	assert_eq!(notifications, 0);
	assert_eq!(highlights, 0);
	assert!(threads.values().all(|counts| *counts == (0, 0)));

	Ok(())
}

fn legacy(kind: &str, state_key: &str, content: &Value) -> Result<RawStrippedState> {
	let event = json!({
		"type": kind, "state_key": state_key, "sender": "@bob:localhost", "content": content,
	});

	wire(&event)
}

/// The knock response entry for an event, decoded as it is off the wire.
///
/// The decoder takes it for a full PDU only when it carries the PDU's auth and
/// prev events, hashes and signatures; anything else is a legacy entry.
fn wire(event: &Value) -> Result<RawStrippedState> {
	let json = to_raw_value(event)?;

	from_str(json.get()).map_err(Into::into)
}

fn knock(room_id: &RoomId, user_id: &UserId) -> Result<PduEvent> {
	let content = json!({"membership": "knock"});
	let event = pdu_json(event_id!("$knock"), room_id, user_id, "m.room.member", &content);

	from_value(with_state_key(event, user_id.as_str())).map_err(Into::into)
}

async fn shown(services: &Services, user_id: &UserId, room_id: &RoomId) -> Result<Vec<Value>> {
	let state = services
		.state_cache
		.knock_state(user_id, room_id)
		.await?;

	json_events(&state)
}

fn cells(shown: &[Value]) -> Vec<(&str, &str)> {
	shown
		.iter()
		.map(|event| {
			let field = |key: &str| event[key].as_str().unwrap_or_default();

			(field("type"), field("state_key"))
		})
		.collect()
}
