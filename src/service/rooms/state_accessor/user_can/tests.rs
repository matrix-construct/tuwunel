//! Tests user authorization decisions against seeded room state.
//!
//! The fixtures exercise redaction boundaries with real service storage and
//! verify that authority does not cross room identities.

use std::sync::Arc;

use futures::StreamExt;
use ruma::{
	EventId, RoomId, UserId, event_id,
	events::{StateEventType, room::member::MembershipState},
	room_id, user_id,
};
use serde_json::{Value, json};
use tuwunel_core::{
	Result,
	config::Figment,
	utils::{IterStream, result::NotFound},
};
use tuwunel_database::Json;
use tuwunel_matrix::{PduCount, PduId, RawPduId};

use crate::{
	Services,
	rooms::{
		short::ShortRoomId, state_cache::MembershipUpdate, state_compressor::CompressedState,
	},
	test_utils::{
		compress_state, fixture, membership_update, pdu_json, save_room_state, set_room_state,
		with_state_key,
	},
};

type StateEvent<'a> = (u64, &'a EventId, &'a str, &'a str, Value);

type Message<'a> = (ShortRoomId, &'a RoomId, u64, &'a EventId);

type Membership<'a> = (&'a RoomId, &'a UserId, MembershipState, u64);

struct Ids {
	room: &'static RoomId,
	foreign: &'static RoomId,
	owner: &'static UserId,
	author: &'static UserId,
	peer: &'static UserId,
	create: &'static EventId,
	local_event: &'static EventId,
	foreign_event: &'static EventId,
	acl: &'static EventId,
	unknown: &'static EventId,
}

#[tokio::test]
async fn redaction_authority_does_not_cross_room_boundaries() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;

	let ids = seed(services).await?;

	for federation in [false, true] {
		for sender in [ids.owner, ids.author, ids.peer] {
			assert!(
				!services
					.state_accessor
					.user_can_redact(ids.foreign_event, sender, ids.room, federation)
					.await?
			);
		}

		for (sender, allowed) in [(ids.owner, true), (ids.author, true), (ids.peer, federation)] {
			let can_redact = services
				.state_accessor
				.user_can_redact(ids.local_event, sender, ids.room, federation)
				.await?;

			assert_eq!(can_redact, allowed);
		}

		assert!(
			services
				.state_accessor
				.user_can_redact(ids.unknown, ids.owner, ids.room, federation)
				.await?
		);

		for protected in [ids.create, ids.acl] {
			services
				.state_accessor
				.user_can_redact(protected, ids.owner, ids.room, federation)
				.await
				.expect_err("protected state event");
		}
	}

	Ok(())
}

#[tokio::test]
async fn equal_hash_state_delta_is_empty() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let ids = seed(services).await?;
	let hash = services
		.state
		.get_room_shortstatehash(ids.room)
		.await?;

	let added: Vec<_> = services
		.state_accessor
		.state_added((hash, hash))
		.collect()
		.await;

	assert_eq!(added, []);

	Ok(())
}

/// A departed member's snapshot is the state after their own leave event.
///
/// The message accepted after the kick carries a snapshot resolved from its
/// own ancestors, as a remote event's would, so it still holds the member as
/// joined plus a later topic, and the room is renamed afterwards. The member
/// reads the kick itself, the earlier topic and no name, and a token from
/// before the kick still reads the state at that position.
///
/// Once the member forgets the room after being admitted, they read neither
/// the current state nor the state after a later token.
#[tokio::test]
async fn departed_member_reads_the_state_after_their_leave_event() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!departed:localhost");
	let owner = user_id!("@owner:localhost");
	let member = user_id!("@member:localhost");
	let create = event_id!("$create:localhost");
	let topic = event_id!("$topic:localhost");
	let join = event_id!("$join:localhost");
	let kick = event_id!("$kick:localhost");
	let retopic = event_id!("$retopic:localhost");
	let next = event_id!("$next:localhost");
	let name = event_id!("$name:localhost");
	let shortroomid = services
		.short
		.get_or_create_shortroomid(room)
		.await;

	let member_key = member.as_str();
	let create_content = json!({ "creator": owner, "room_version": "10" });
	let state_events = [
		(1, create, "m.room.create", "", create_content),
		(2, topic, "m.room.topic", "", json!({ "topic": "before" })),
		(3, join, "m.room.member", member_key, json!({ "membership": "join" })),
		(4, kick, "m.room.member", member_key, json!({ "membership": "leave" })),
		(6, name, "m.room.name", "", json!({ "name": "after" })),
	];

	store_state(services, shortroomid, room, owner, &state_events);

	let content = json!({ "msgtype": "m.text", "body": "later" });

	store_messages(services, owner, &content, &[(shortroomid, room, 5, next)]);

	let content = json!({ "topic": "after" });
	let pdu = with_state_key(pdu_json(retopic, room, owner, "m.room.topic", &content), "");

	outlier(services, retopic, &pdu);

	let created = compress_state(services, &StateEventType::RoomCreate, "", create).await;
	let topic_before = compress_state(services, &StateEventType::RoomTopic, "", topic).await;
	let topic_after = compress_state(services, &StateEventType::RoomTopic, "", retopic).await;
	let joined = compress_state(services, &StateEventType::RoomMember, member_key, join).await;
	let kicked = compress_state(services, &StateEventType::RoomMember, member_key, kick).await;
	let named = compress_state(services, &StateEventType::RoomName, "", name).await;

	let at_kick = services
		.state
		.set_event_state(kick, room, Arc::new([created, topic_before, joined].into()))
		.await?;

	services
		.state
		.set_event_state(next, room, Arc::new([created, topic_after, joined].into()))
		.await?;

	let state = Arc::new([created, topic_before, kicked, named].into());
	let current = save_room_state(services, room, state).await?;

	let memberships = [
		(room, member, MembershipState::Join, 3),
		(room, member, MembershipState::Leave, 4),
	];

	set_memberships(services, owner, memberships).await?;

	let accessor = &services.state_accessor;
	let visible = accessor
		.user_visible_shortstatehash(member, room, None)
		.await?
		.expect("departed member may read state");

	let membership = accessor
		.state_get_id(visible, &StateEventType::RoomMember, member_key)
		.await?;

	let visible_topic = accessor
		.state_get_id(visible, &StateEventType::RoomTopic, "")
		.await?;

	let visible_name = accessor
		.state_get_id(visible, &StateEventType::RoomName, "")
		.await
		.optional()?;

	assert_eq!(membership, kick);
	assert_eq!(visible_topic, topic);
	assert_eq!(visible_name, None, "a departed member reads a later name");
	assert_ne!(visible, current);

	let later = accessor
		.user_visible_shortstatehash(member, room, Some(PduCount::Normal(6)))
		.await?
		.expect("departed member may read state");

	let earlier = accessor
		.user_visible_shortstatehash(member, room, Some(PduCount::Normal(3)))
		.await?
		.expect("departed member may read state");

	assert_eq!(later, visible, "a later token reads past the kick");
	assert_eq!(earlier, at_kick, "an earlier token reads past its position");

	assert!(
		accessor
			.user_can_see_state_events(member, room)
			.await
	);

	services.state_cache.forget(room, member);

	let forgotten = accessor
		.user_visible_shortstatehash(member, room, None)
		.await?;

	let later = accessor
		.user_visible_shortstatehash(member, room, Some(PduCount::Normal(6)))
		.await?;

	assert_eq!(forgotten, None, "a forgotten member reads current state");
	assert_eq!(later, None, "a forgotten member reads state after a later token");

	Ok(())
}

/// An event without recorded state is judged by the room's current state, and
/// one whose recorded state cannot be read is hidden.
///
/// A message stored without a snapshot, as a backfilled event is, sits in a
/// room that sets no history visibility, so history defaults to `shared`. The
/// owner and a member who left after it see it and a never-member does not,
/// while in a room with no state at all nobody does.
#[tokio::test]
async fn snapshotless_event_is_judged_by_current_state() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!snapshotless:localhost");
	let stateless = room_id!("!stateless:localhost");
	let owner = user_id!("@owner:localhost");
	let member = user_id!("@member:localhost");
	let stranger = user_id!("@stranger:localhost");
	let create = event_id!("$create:localhost");
	let join = event_id!("$join:localhost");
	let leave = event_id!("$leave:localhost");
	let message = event_id!("$message:localhost");
	let orphan = event_id!("$orphan:localhost");
	let unreadable = event_id!("$unreadable:localhost");
	let shortroomid = services
		.short
		.get_or_create_shortroomid(room)
		.await;

	let stateless_shortroomid = services
		.short
		.get_or_create_shortroomid(stateless)
		.await;

	let owner_key = owner.as_str();
	let member_key = member.as_str();
	let create_content = json!({ "creator": owner, "room_version": "10" });
	let state_events = [
		(1, create, "m.room.create", "", create_content),
		(2, join, "m.room.member", owner_key, json!({ "membership": "join" })),
		(4, leave, "m.room.member", member_key, json!({ "membership": "leave" })),
	];

	store_state(services, shortroomid, room, owner, &state_events);

	let content = json!({ "msgtype": "m.text", "body": "no snapshot" });
	let messages = [
		(shortroomid, room, 3, message),
		(stateless_shortroomid, stateless, 1, orphan),
		(shortroomid, room, 5, unreadable),
	];

	store_messages(services, owner, &content, &messages);

	let shorteventid = services
		.short
		.get_or_create_shorteventid(unreadable)
		.await;

	services.db["shorteventid_shortstatehash"].insert(&shorteventid.to_be_bytes(), [0xFF_u8]);

	let state = compress_states(services, &state_events).await;

	save_room_state(services, room, Arc::new(state)).await?;

	let memberships = [
		(room, member, MembershipState::Join, 2),
		(room, member, MembershipState::Leave, 4),
		(stateless, owner, MembershipState::Join, 1),
	];

	set_memberships(services, owner, memberships).await?;

	let accessor = &services.state_accessor;
	let message = services.timeline.get_pdu(message).await?;
	let orphan = services.timeline.get_pdu(orphan).await?;
	let unreadable = services.timeline.get_pdu(unreadable).await?;

	assert!(accessor.user_can_see_event(owner, &message).await, "a member loses the event");
	assert!(
		accessor
			.user_can_see_event(member, &message)
			.await,
		"a departed member loses history from before they left"
	);

	assert!(
		!accessor
			.user_can_see_event(stranger, &message)
			.await,
		"a never-member sees the event"
	);

	assert!(
		!accessor.user_can_see_event(owner, &orphan).await,
		"an event in a room without state is visible"
	);

	assert!(
		!accessor
			.user_can_see_event(owner, &unreadable)
			.await,
		"an event whose state cannot be read is judged by later state"
	);

	Ok(())
}

async fn seed(services: &Services) -> Result<Ids> {
	let ids = Ids {
		room: room_id!("!redaction:localhost"),
		foreign: room_id!("!other:localhost"),
		owner: user_id!("@owner:localhost"),
		author: user_id!("@author:remote.invalid"),
		peer: user_id!("@peer:remote.invalid"),
		create: event_id!("$create:localhost"),
		local_event: event_id!("$local:localhost"),
		foreign_event: event_id!("$foreign:localhost"),
		acl: event_id!("$acl:localhost"),
		unknown: event_id!("$unknown:localhost"),
	};

	let content = json!({
		"creator": ids.owner, "room_version": "10",
	});

	let create =
		with_state_key(pdu_json(ids.create, ids.room, ids.owner, "m.room.create", &content), "");

	services.db["eventid_outlierpdu"].raw_put(ids.create, Json(create));
	let content = json!({
		"allow": ["*"], "deny": [], "allow_ip_literals": false,
	});

	let acl =
		with_state_key(pdu_json(ids.acl, ids.room, ids.owner, "m.room.server_acl", &content), "");

	services.db["eventid_outlierpdu"].raw_put(ids.acl, Json(acl));

	for (id, room) in [(ids.local_event, ids.room), (ids.foreign_event, ids.foreign)] {
		let content = json!({
			"msgtype": "m.text", "body": "preserve this message",
		});

		let pdu = pdu_json(id, room, ids.author, "m.room.message", &content);

		outlier(services, id, &pdu);
	}

	let create = compress_state(services, &StateEventType::RoomCreate, "", ids.create).await;
	let state = Arc::new([create].into());
	let state = services
		.state
		.set_event_state(ids.create, ids.room, state)
		.await?;

	set_room_state(services, ids.room, state).await;

	Ok(ids)
}

fn store_state(
	services: &Services,
	shortroomid: ShortRoomId,
	room_id: &RoomId,
	sender: &UserId,
	events: &[StateEvent<'_>],
) {
	for (count, id, kind, state_key, content) in events {
		let pdu = with_state_key(pdu_json(id, room_id, sender, kind, content), state_key);

		store(services, shortroomid, *count, id, &pdu);
	}
}

fn store_messages(
	services: &Services,
	sender: &UserId,
	content: &Value,
	messages: &[Message<'_>],
) {
	for &(shortroomid, room_id, count, id) in messages {
		let pdu = pdu_json(id, room_id, sender, "m.room.message", content);

		store(services, shortroomid, count, id, &pdu);
	}
}

async fn compress_states(services: &Services, events: &[StateEvent<'_>]) -> CompressedState {
	events
		.iter()
		.stream()
		.then(async |&(_, id, kind, state_key, _)| {
			compress_state(services, &StateEventType::from(kind), state_key, id).await
		})
		.collect()
		.await
}

async fn set_memberships<'a, Memberships>(
	services: &Services,
	sender: &UserId,
	memberships: Memberships,
) -> Result
where
	Memberships: IntoIterator<Item = Membership<'a>>,
{
	for (room_id, user_id, membership, count) in memberships {
		let update = MembershipUpdate {
			sender,
			update_joined_count: false,
			..membership_update(room_id, user_id, membership, PduCount::Normal(count))
		};

		services
			.state_cache
			.update_membership(update)
			.await?;
	}

	Ok(())
}

fn store(services: &Services, shortroomid: ShortRoomId, count: u64, id: &EventId, pdu: &Value) {
	let pdu_id: RawPduId = PduId {
		shortroomid,
		count: PduCount::Normal(count),
	}
	.into();

	services.db["eventid_pduid"].insert(id.as_bytes(), pdu_id.as_bytes());
	services.db["pduid_pdu"].raw_put(pdu_id, Json(pdu));
}

fn outlier(services: &Services, id: &EventId, pdu: &Value) {
	services.db["eventid_outlierpdu"].raw_put(id, Json(pdu));
}
