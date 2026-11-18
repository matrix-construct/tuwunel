use ruma::{RoomVersionId, event_id, room_id, user_id};
use serde_json::{from_value, json};
use tuwunel_core::Result;
use tuwunel_matrix::{PduEvent, room_version};

use super::check_create_reference;
use crate::test_utils::{pdu_json, with_state_key};

#[test]
fn a_known_create_is_the_room_anchor() -> Result {
	let id = event_id!("$create");
	let room = room_id!("!room:example.com");
	let alice = user_id!("@alice:example.com");
	let create: PduEvent = from_value(with_state_key(pdu_json(id, room, alice, "m.room.create", &json!({})), ""))?;
	let rules = room_version::rules(&RoomVersionId::V11)?;
	check_create_reference(&create, &rules, id)?;
	check_create_reference(&create, &rules, event_id!("$different")).expect_err("different create ID");

	let mut name: PduEvent = from_value(with_state_key(pdu_json(event_id!("$name"), room, alice, "m.room.name", &json!({ "name": "Example" })), ""))?;
	name.auth_events.push(id.to_owned());
	check_create_reference(&name, &rules, id)?;
	check_create_reference(&name, &rules, event_id!("$different")).expect_err("different auth anchor");
	Ok(())
}
