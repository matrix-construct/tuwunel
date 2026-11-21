use ruma::{
	EventId, RoomVersionId, api::error::ErrorKind, event_id, events::StateEventType, room_id,
	user_id,
};
use serde_json::{from_str, from_value, json};
use tuwunel_core::{Result, config::Figment};

use super::{Assembly, StateIdsResponse, distinct};
use crate::test_utils::{fixture, pdu_json, with_state_key};

#[test]
fn state_ids_keep_each_event_once() -> Result {
	let StateIdsResponse { pdu_ids } = from_str(r#"{"pdu_ids":["$one","$two","$one"]}"#)?;
	let (pdu_ids, repeated) = distinct(pdu_ids);

	assert_eq!(pdu_ids, [event_id!("$one"), event_id!("$two")]);
	assert_eq!(repeated, 1, "one repeated ID is dropped");

	Ok(())
}

#[tokio::test]
async fn a_repeated_state_id_assembles_and_a_conflict_is_refused() -> Result {
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
	let seeds = [
		(create, "m.room.create", "", json!({})),
		(joined, "m.room.member", member, json!({"membership": "join"})),
		(left, "m.room.member", member, json!({"membership": "leave"})),
	];

	for (event_id, kind, state_key, content) in seeds {
		let event = pdu_json(event_id, room_id, sender, kind, &content);

		services
			.timeline
			.add_pdu_outlier(event_id, &from_value(with_state_key(event, state_key))?);
	}

	let assemble = async |ids: [&EventId; 3]| {
		let ids = ids.map(ToOwned::to_owned).into();
		let assembly = Assembly {
			origin: sender.server_name(),
			responder: sender.server_name(),
			room_id,
			room_version: &RoomVersionId::V11,
			recursion_level: 0,
			create_event_id: create,
		};

		services
			.event_handler
			.assemble_state(ids, assembly)
			.await
	};

	let state = assemble([create, joined, joined]).await?;
	let create_key = services
		.short
		.get_shortstatekey(&StateEventType::RoomCreate, "")
		.await?;

	let member_key = services
		.short
		.get_shortstatekey(&StateEventType::RoomMember, member)
		.await?;

	assert_eq!(state, [(create_key, create.to_owned()), (member_key, joined.to_owned())].into());

	let conflict = assemble([create, joined, left])
		.await
		.expect_err("two events for one state key cannot both be state");

	let message = conflict.message();

	assert_eq!(conflict.kind(), ErrorKind::InvalidParam, "{conflict}");
	assert!(message.contains("exists multiple times"), "{conflict}");

	Ok(())
}
