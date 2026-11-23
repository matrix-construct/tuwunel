//! Tests visibility decisions when a read on the way fails.
//!
//! Each case damages one stored row on the decision's path, such as a timeline
//! position, a snapshot pointer or a member event, and compares the fallible
//! accessor's result with the boolean accessor's decision.

use std::sync::Arc;

use ruma::{
	EventId, RoomId, UserId, event_id, events::room::member::MembershipState, room_id, user_id,
};
use serde_json::{Value, json};
use tuwunel_core::{Result, config::Figment, utils::time::now_millis};
use tuwunel_matrix::{
	Event,
	event::{STICKY_DURATION_MAX, STICKY_KEY},
};

use super::{compress_states, set_memberships, store, store_messages, store_state};
use crate::test_utils::{fixture, pdu_json, save_room_state, with_field};

/// A timeline position that does not decode fails the visibility read that
/// reaches it, where a live reader would panic.
///
/// The position is read for a former member's `shared` history, for a sticky
/// event's index row once history denies it, and for a history visibility
/// event a snapshot names. `try_user_can_see_event` returns an error for each.
/// `user_can_see_event` denies the former member's message and the late
/// joiner's sticky event, and takes the unreadable history visibility for
/// `shared`, which lets the owner see the message.
/// A user not joined is denied the sticky event outright, since membership
/// settles what the failed index read left open.
#[tokio::test]
async fn damaged_event_position_is_a_failed_read() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!damaged:localhost");
	let owner = user_id!("@owner:localhost");
	let member = user_id!("@member:localhost");
	let late = user_id!("@late:localhost");
	let stranger = user_id!("@stranger:localhost");
	let create = event_id!("$create:localhost");
	let join = event_id!("$join:localhost");
	let leave = event_id!("$leave:localhost");
	let closed = event_id!("$closed:localhost");
	let hidden = event_id!("$hidden:localhost");
	let message = event_id!("$message:localhost");
	let sticky = event_id!("$sticky:localhost");
	let quiet = event_id!("$quiet:localhost");
	let shortroomid = services
		.short
		.get_or_create_shortroomid(room)
		.await;

	let create_content = json!({ "creator": owner, "room_version": "10" });
	let joined = json!({ "history_visibility": "joined" });
	let state_events = [
		(1, create, "m.room.create", "", create_content),
		(2, join, "m.room.member", owner.as_str(), json!({ "membership": "join" })),
		(4, leave, "m.room.member", member.as_str(), json!({ "membership": "leave" })),
		(5, closed, "m.room.history_visibility", "", joined.clone()),
		(6, hidden, "m.room.history_visibility", "", joined),
	];

	store_state(services, shortroomid, room, owner, &state_events);

	let content = json!({ "msgtype": "m.text", "body": "position" });
	let messages = [(shortroomid, room, 3, message), (shortroomid, room, 8, quiet)];

	store_messages(services, owner, &content, &messages);
	store(services, shortroomid, 7, sticky, &sticky_message(sticky, room, owner));

	let current = compress_states(services, &state_events[..3]).await;
	let closing = &current | &compress_states(services, &state_events[3..4]).await;
	let hiding = &current | &compress_states(services, &state_events[4..]).await;

	save_room_state(services, room, Arc::new(current)).await?;

	for (id, state) in [(sticky, closing), (quiet, hiding)] {
		services
			.state
			.set_event_state(id, room, Arc::new(state))
			.await?;
	}

	let memberships = [
		(room, owner, MembershipState::Join, 2),
		(room, member, MembershipState::Join, 2),
		(room, member, MembershipState::Leave, 4),
		(room, late, MembershipState::Join, 9),
	];

	set_memberships(services, owner, memberships).await?;

	let accessor = &services.state_accessor;
	let message = services.timeline.get_pdu(message).await?;
	let sticky = services.timeline.get_pdu(sticky).await?;
	let quiet = services.timeline.get_pdu(quiet).await?;

	for id in [message.event_id(), sticky.event_id(), hidden] {
		services.db["eventid_pduid"].insert(id.as_bytes(), [0xFF_u8]);
	}

	let cases = [
		(member, &message, "a former member's history"),
		(late, &sticky, "a sticky event history denies"),
		(owner, &quiet, "a damaged history visibility"),
	];

	for (user_id, pdu, case) in cases {
		let seen = accessor
			.try_user_can_see_event(user_id, pdu)
			.await;

		assert!(seen.is_err(), "{case}: a damaged position read as a decision");
	}

	let member_seen = accessor
		.user_can_see_event(member, &message)
		.await;

	assert!(!member_seen, "a damaged position kept a former member's history");

	assert!(
		!accessor.user_can_see_event(late, &sticky).await,
		"a damaged position granted a sticky event"
	);

	let stranger_seen = accessor
		.try_user_can_see_event(stranger, &sticky)
		.await?;

	assert!(!stranger_seen, "a failed index read outweighed membership's denial");

	assert!(
		accessor.user_can_see_event(owner, &quiet).await,
		"an unreadable history visibility was not taken for shared"
	);

	Ok(())
}

fn sticky_message(id: &EventId, room_id: &RoomId, sender: &UserId) -> Value {
	let content = json!({ "msgtype": "m.text", "body": "sticky" });
	let pdu = pdu_json(id, room_id, sender, "m.room.message", &content);
	let pdu = with_field(pdu, "origin_server_ts", json!(now_millis()));

	with_field(pdu, STICKY_KEY, json!({ "duration_ms": STICKY_DURATION_MAX }))
}
