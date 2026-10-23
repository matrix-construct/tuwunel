//! Tests user authorization decisions against seeded room state.
//!
//! The fixtures exercise redaction boundaries with real service storage and
//! verify that authority does not cross room identities.

use std::sync::Arc;

use futures::StreamExt;
use ruma::{
	EventId, RoomId, UserId, event_id,
	events::{
		StateEventType,
		room::member::{MembershipState, RoomMemberEventContent},
	},
	room_id, user_id,
};
use serde_json::{Value, json};
use tuwunel_core::{Result, config::Figment, utils::result::NotFound};
use tuwunel_database::Json;
use tuwunel_matrix::{PduCount, PduId, RawPduId};

use crate::{
	Services,
	rooms::{short::ShortRoomId, state_cache::MembershipUpdate},
	test_utils::{compress_state, fixture, pdu_json, with_state_key},
};

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

	for (count, id, kind, state_key, content) in &state_events {
		let pdu = with_state_key(pdu_json(id, room, owner, kind, content), state_key);

		store(services, shortroomid, *count, id, &pdu);
	}

	let content = json!({ "msgtype": "m.text", "body": "later" });
	let pdu = pdu_json(next, room, owner, "m.room.message", &content);

	store(services, shortroomid, 5, next, &pdu);

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

	let current = services
		.state_compressor
		.save_state(room, Arc::new([created, topic_before, kicked, named].into()))
		.await?
		.shortstatehash;

	services
		.state
		.set_room_state(room, current, &services.state.mutex.lock(room).await);

	for (membership, count) in [(MembershipState::Join, 3), (MembershipState::Leave, 4)] {
		services
			.state_cache
			.update_membership(MembershipUpdate {
				room_id: room,
				user_id: member,
				membership_event: RoomMemberEventContent::new(membership),
				sender: owner,
				last_state: None,
				invite_via: None,
				update_joined_count: false,
				count: PduCount::Normal(count),
			})
			.await?;
	}

	let accessor = &services.state_accessor;
	let visible = accessor
		.user_visible_shortstatehash(member, room, None)
		.await?;

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
		.await?;

	let earlier = accessor
		.user_visible_shortstatehash(member, room, Some(PduCount::Normal(3)))
		.await?;

	assert_eq!(later, visible, "a later token reads past the kick");
	assert_eq!(earlier, at_kick, "an earlier token reads past its position");

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

	let lock = services.state.mutex.lock(ids.room).await;

	services
		.state
		.set_room_state(ids.room, state, &lock);

	Ok(ids)
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
