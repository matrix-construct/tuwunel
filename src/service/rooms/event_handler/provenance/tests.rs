use ruma::{RoomVersionId, event_id, room_id, server_name, user_id};
use serde_json::{from_value, json};
use tuwunel_core::Result;
use tuwunel_matrix::{PduEvent, room_version};

use super::{check_create_reference, check_local_join_sender};
use crate::test_utils::{pdu_json, with_state_key};

#[test]
fn a_known_create_is_the_room_anchor() -> Result {
	let id = event_id!("$create");
	let room = room_id!("!room:example.com");
	let alice = user_id!("@alice:example.com");
	let create: PduEvent =
		from_value(with_state_key(pdu_json(id, room, alice, "m.room.create", &json!({})), ""))?;
	let rules = room_version::rules(&RoomVersionId::V11)?;
	check_create_reference(&create, &rules, id)?;
	check_create_reference(&create, &rules, event_id!("$different"))
		.expect_err("different create ID");

	let mut name: PduEvent = from_value(with_state_key(
		pdu_json(event_id!("$name"), room, alice, "m.room.name", &json!({ "name": "Example" })),
		"",
	))?;
	name.auth_events.push(id.to_owned());
	check_create_reference(&name, &rules, id)?;
	check_create_reference(&name, &rules, event_id!("$different"))
		.expect_err("different auth anchor");
	Ok(())
}

#[test]
fn a_local_join_is_self_sent() -> Result {
	let alice = user_id!("@alice:example.com");
	let local = server_name!("example.com");
	let member = |key: &str, membership: &str| -> Result<PduEvent> {
		Ok(from_value(with_state_key(
			pdu_json(
				event_id!("$member"),
				room_id!("!room:example.com"),
				alice,
				"m.room.member",
				&json!({ "membership": membership }),
			),
			key,
		))?)
	};
	check_local_join_sender(&member(alice.as_str(), "join")?, local)?;
	check_local_join_sender(&member("@bob:example.com", "invite")?, local)?;
	check_local_join_sender(&member("@bob:example.com", "join")?, local)
		.expect_err("different local user");
	Ok(())
}
