//! Event rows holding a further record after the event's.
//!
//! A debug build of the live event readers panics on such a row, where a release
//! build reads the event and ignores the rest. Each check reaching an event reads
//! it as a release build does.

use ruma::{EventId, event_id, room_id, user_id};
use serde_json::{Value, json};
use tuwunel_core::{Result, config::Figment};
use tuwunel_matrix::PduCount;

use super::{room_pdu_id, seed_room_state, walk};
use crate::{
	migrations::fresh,
	test_utils::{fixture, pdu_id, pdu_json, with_state_key},
};

#[tokio::test]
async fn joined_members_read_member_events_with_a_trailing_record() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!room:localhost");
	let (alice, bob) = (user_id!("@alice:localhost"), user_id!("@bob:localhost"));
	let (alice_member, bob_member) = (event_id!("$alice:localhost"), event_id!("$bob:localhost"));
	let joined = json!({ "membership": "join" });
	let create = json!({ "creator": alice, "room_version": "10" });
	let state = [
		(1, event_id!("$create:localhost"), "m.room.create", "", create),
		(2, alice_member, "m.room.member", alice.as_str(), joined.clone()),
		(3, bob_member, "m.room.member", bob.as_str(), joined.clone()),
	];

	fresh(services).await?;
	seed_room_state(services, room, alice, &state).await?;

	let shortroomid = services.short.get_shortroomid(room).await?;
	let member = |id: &EventId, user: &str| {
		let pdu = pdu_json(id, room, alice, "m.room.member", &joined);

		trailing(&with_state_key(pdu, user))
	};

	// Alice's accepted row and Bob's outlier copy, his only one, both carry a record more.
	let (alice_row, bob_row) =
		(member(alice_member, alice.as_str()), member(bob_member, bob.as_str()));

	services.db["pduid_pdu"].insert(&room_pdu_id(shortroomid, 2), alice_row);
	services.db["eventid_pduid"].remove(bob_member.as_bytes());
	services.db["eventid_outlierpdu"].insert(bob_member.as_bytes(), bob_row);

	for user in [alice, bob] {
		services
			.state_cache
			.mark_as_joined(user, room, PduCount::Normal(4));
	}

	let summary = walk(services, "joined-members").await?;

	assert_eq!(summary.findings(), 0);

	Ok(())
}

#[tokio::test]
async fn thread_summaries_read_rows_with_a_trailing_record() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!thread:localhost");
	let (root, reply) = (event_id!("$root:localhost"), event_id!("$reply:localhost"));
	let alice = user_id!("@alice:localhost");
	let thread = json!({ "m.relations": { "m.thread": { "count": 1 } } });
	let relates = json!({ "m.relates_to": { "rel_type": "m.thread", "event_id": root } });
	let root_pdu = json!({ "room_id": room, "unsigned": thread });
	let reply_pdu = pdu_json(reply, room, alice, "m.room.message", &relates);
	let relation =
		[PduCount::Normal(1).to_be_bytes(), PduCount::Normal(2).to_be_bytes()].concat();

	fresh(services).await?;
	services.db["pduid_pdu"].insert(&pdu_id(1), trailing(&root_pdu));
	services.db["pduid_pdu"].insert(&pdu_id(2), trailing(&reply_pdu));
	services.db["threadid_userids"].insert(&pdu_id(1), alice.as_str());
	services.db["tofrom_relation"].insert(&relation, []);

	let summary = walk(services, "thread-summaries").await?;

	assert_eq!(summary.findings(), 0);

	Ok(())
}

fn trailing(pdu: &Value) -> Vec<u8> {
	let pdu = serde_json::to_vec(pdu).expect("an event serializes");

	[pdu.as_slice(), &[0xFF, 0]].concat()
}
