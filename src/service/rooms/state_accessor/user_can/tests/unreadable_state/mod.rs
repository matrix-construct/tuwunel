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

use super::{
	compress_states, set_memberships, set_predecessor_join, store, store_messages, store_state,
};
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

	set_predecessor_join(services, room, member, leave).await?;

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

	services.db["eventid_pduid"].insert(leave.as_bytes(), [0xFF_u8]);
	let err = accessor
		.try_user_can_see_event(member, &message)
		.await
		.expect_err("a damaged departure position read as a decision");
	assert!(err.to_string().contains(leave.as_str()));
	let seen = accessor
		.user_can_see_event(member, &message)
		.await;
	assert!(!seen);

	Ok(())
}

/// A joined user's sticky event is visible even when its recorded state cannot
/// be read.
///
/// The event is unexpired and indexed, and its snapshot pointer is corrupt. Both
/// entry points grant it to the owner. A stranger is still denied, and to
/// `try_user_can_see_event` the failed read stays an error.
#[tokio::test]
async fn sticky_grant_outlasts_an_unreadable_snapshot() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!pinned:localhost");
	let owner = user_id!("@owner:localhost");
	let stranger = user_id!("@stranger:localhost");
	let create = event_id!("$create:localhost");
	let join = event_id!("$join:localhost");
	let pinned = event_id!("$pinned:localhost");
	let shortroomid = services
		.short
		.get_or_create_shortroomid(room)
		.await;

	let create_content = json!({ "creator": owner, "room_version": "10" });
	let state_events = [
		(1, create, "m.room.create", "", create_content),
		(2, join, "m.room.member", owner.as_str(), json!({ "membership": "join" })),
	];

	store_state(services, shortroomid, room, owner, &state_events);
	store(services, shortroomid, 3, pinned, &sticky_message(pinned, room, owner));

	let state = compress_states(services, &state_events).await;

	save_room_state(services, room, Arc::new(state)).await?;
	set_memberships(services, owner, [(room, owner, MembershipState::Join, 2)]).await?;

	let pdu_id = services.timeline.get_pdu_id(pinned).await?;
	let shorteventid = services
		.short
		.get_or_create_shorteventid(pinned)
		.await;

	let pinned = services.timeline.get_pdu(pinned).await?;
	let until = pinned
		.sticky_until(now_millis())
		.expect("the event is sticky");

	services.db["pduid_stickyuntil"].insert(pdu_id.as_bytes(), until.to_be_bytes());
	services.db["shorteventid_shortstatehash"].insert(&shorteventid.to_be_bytes(), [0xFF_u8]);

	let accessor = &services.state_accessor;
	let owner_seen = accessor
		.try_user_can_see_event(owner, &pinned)
		.await?;

	assert!(owner_seen, "an unreadable snapshot outweighed the sticky grant");

	assert!(
		accessor.user_can_see_event(owner, &pinned).await,
		"an unreadable snapshot outweighed the sticky grant"
	);

	let stranger_seen = accessor
		.try_user_can_see_event(stranger, &pinned)
		.await;

	assert!(stranger_seen.is_err(), "a failed snapshot read became a decision");

	let stranger_granted = accessor
		.user_can_see_event(stranger, &pinned)
		.await;

	assert!(!stranger_granted, "a sticky event reached a user not joined");

	Ok(())
}

/// A former member keeps an event from before their leave though their member
/// event in its snapshot cannot be loaded.
///
/// Under `shared`, a leave falling after the event grants it, so the failed
/// read of the membership at the event decides nothing.
#[tokio::test]
async fn departure_grants_history_past_an_unreadable_membership() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!departed:localhost");
	let owner = user_id!("@owner:localhost");
	let member = user_id!("@member:localhost");
	let create = event_id!("$create:localhost");
	let join = event_id!("$join:localhost");
	let entered = event_id!("$entered:localhost");
	let leave = event_id!("$leave:localhost");
	let early = event_id!("$early:localhost");
	let shortroomid = services
		.short
		.get_or_create_shortroomid(room)
		.await;

	let create_content = json!({ "creator": owner, "room_version": "10" });
	let state_events = [
		(1, create, "m.room.create", "", create_content),
		(2, join, "m.room.member", owner.as_str(), json!({ "membership": "join" })),
		(3, entered, "m.room.member", member.as_str(), json!({ "membership": "join" })),
		(5, leave, "m.room.member", member.as_str(), json!({ "membership": "leave" })),
	];

	store_state(services, shortroomid, room, owner, &state_events);

	let content = json!({ "msgtype": "m.text", "body": "before leaving" });

	store_messages(services, owner, &content, &[(shortroomid, room, 4, early)]);

	let base = compress_states(services, &state_events[..2]).await;
	let state = Arc::new(&base | &compress_states(services, &state_events[2..3]).await);
	let current = &base | &compress_states(services, &state_events[3..]).await;

	save_room_state(services, room, Arc::new(current)).await?;
	services
		.state
		.set_event_state(early, room, state)
		.await?;

	set_predecessor_join(services, room, member, leave).await?;

	let memberships = [
		(room, owner, MembershipState::Join, 2),
		(room, member, MembershipState::Join, 3),
		(room, member, MembershipState::Leave, 5),
	];

	set_memberships(services, owner, memberships).await?;

	let pdu_id = services.timeline.get_pdu_id(entered).await?;

	services.db["pduid_pdu"].remove(&pdu_id);

	let accessor = &services.state_accessor;
	let early = services.timeline.get_pdu(early).await?;
	let seen = accessor
		.try_user_can_see_event(member, &early)
		.await?;

	assert!(seen, "an unreadable membership at the event outweighed the departure");

	assert!(
		accessor.user_can_see_event(member, &early).await,
		"an unreadable membership at the event outweighed the departure"
	);

	Ok(())
}

fn sticky_message(id: &EventId, room_id: &RoomId, sender: &UserId) -> Value {
	let content = json!({ "msgtype": "m.text", "body": "sticky" });
	let pdu = pdu_json(id, room_id, sender, "m.room.message", &content);
	let pdu = with_field(pdu, "origin_server_ts", json!(now_millis()));

	with_field(pdu, STICKY_KEY, json!({ "duration_ms": STICKY_DURATION_MAX }))
}
