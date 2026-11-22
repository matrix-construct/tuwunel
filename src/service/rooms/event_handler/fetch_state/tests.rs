use ruma::{
	EventId, RoomVersionId, api::error::ErrorKind, event_id, events::StateEventType, room_id,
	user_id,
};
use serde_json::{from_str, from_value, json};
use tuwunel_core::{Error, Result, config::Figment};

use super::{super::eval::Eval, StateIdsResponse, distinct, room_state_key};
use crate::test_utils::{fixture, pdu_json, with_state_key};

#[test]
fn state_ids_keep_each_event_once() -> Result {
	let StateIdsResponse { pdu_ids } = from_str(r#"{"pdu_ids":["$one","$two","$one"]}"#)?;
	let (pdu_ids, repeated) = distinct(pdu_ids);

	assert_eq!(pdu_ids, [event_id!("$one"), event_id!("$two")]);
	assert_eq!(repeated, 1, "one repeated ID is dropped");

	Ok(())
}

#[test]
fn a_malformed_state_id_fails_the_response() {
	let response: Result<StateIdsResponse, _> = from_str(r#"{"pdu_ids":["$one","one","$one"]}"#);

	assert!(response.is_err(), "a malformed ID among repeats fails the whole response");
}

#[test]
fn a_fetched_state_key_is_scoped_to_its_room() -> Result {
	let room = room_id!("!one:example.com");
	let name = pdu_json(
		event_id!("$name"),
		room,
		user_id!("@alice:example.com"),
		"m.room.name",
		&json!({ "name": "Room one" }),
	);

	let pdu = from_value(with_state_key(name, ""))?;

	assert_eq!(room_state_key(&pdu, room)?, "");
	room_state_key(&pdu, room_id!("!two:example.com"))
		.expect_err("a state key cannot be taken from another room");

	Ok(())
}

#[tokio::test]
async fn a_repeated_state_id_assembles_and_bad_state_is_refused() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!state:localhost");
	let sender = user_id!("@bob:localhost");
	let member = sender.as_str();
	let create = event_id!("$create");
	let joined = event_id!("$joined");
	let left = event_id!("$left");
	let other_create = event_id!("$other");
	let foreign = event_id!("$foreign");
	let chat = event_id!("$chat");
	let other_room = room_id!("!other:localhost");
	let seeds = [
		(create, room_id, "m.room.create", Some(""), json!({})),
		(other_create, room_id, "m.room.create", Some(""), json!({})),
		(joined, room_id, "m.room.member", Some(member), json!({"membership": "join"})),
		(left, room_id, "m.room.member", Some(member), json!({"membership": "leave"})),
		(foreign, other_room, "m.room.name", Some(""), json!({"name": "other"})),
		(chat, room_id, "m.room.message", None, json!({"body": "hello"})),
	];

	for (event_id, room, kind, state_key, content) in seeds {
		let event = pdu_json(event_id, room, sender, kind, &content);
		let event = match state_key {
			| Some(state_key) => with_state_key(event, state_key),
			| None => event,
		};

		services
			.timeline
			.add_pdu_outlier(event_id, &from_value(event)?);
	}

	let assemble = async |ids: &[&EventId]| {
		let ids = ids
			.iter()
			.copied()
			.map(ToOwned::to_owned)
			.collect();

		let origin = sender.server_name();
		let eval = Eval {
			origin,
			room_id,
			event_id: event_id!("$incoming"),
			room_version: &RoomVersionId::V11,
			create_event_id: create,
			recursion_level: 0,
			auth_events_known: false,
		};

		services
			.event_handler
			.assemble_state(ids, eval, origin)
			.await
	};

	let state = assemble(&[create, joined, joined]).await?;
	let create_key = services
		.short
		.get_shortstatekey(&StateEventType::RoomCreate, "")
		.await?;

	let member_key = services
		.short
		.get_shortstatekey(&StateEventType::RoomMember, member)
		.await?;

	assert_eq!(state, [(create_key, create.to_owned()), (member_key, joined.to_owned())].into());

	let conflict = assemble(&[create, joined, left])
		.await
		.expect_err("two events for one state key cannot both be state");

	let message = conflict.message();

	assert_eq!(conflict.kind(), ErrorKind::InvalidParam, "{conflict}");
	assert!(message.contains("exists multiple times"), "{conflict}");

	let repeated_create = assemble(&[create, create, joined]).await?;

	assert_eq!(repeated_create, state);

	let lone_create = assemble(&[create]).await?;

	assert_eq!(lone_create, [(create_key, create.to_owned())].into());

	let without_original_create: [&[&EventId]; 3] = [&[], &[joined], &[other_create, joined]];

	for ids in without_original_create {
		let error = assemble(ids)
			.await
			.expect_err("the state must hold the original create event");

		assert!(
			matches!(&error, Error::Database(message) if message.contains("wrong create event")),
			"{error}"
		);
	}

	let error = assemble(&[create, foreign])
		.await
		.expect_err("an event of another room cannot be this room's state");

	assert_eq!(error.kind(), ErrorKind::InvalidParam, "{error}");
	assert!(error.message().contains("wrong room"), "{error}");

	let error = assemble(&[create, chat])
		.await
		.expect_err("a message cannot be state");

	assert!(
		matches!(&error, Error::Database(message) if message.contains("non-state pdu")),
		"{error}"
	);

	Ok(())
}
