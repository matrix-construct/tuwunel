use std::sync::Arc;

use ruma::{
	OwnedServerName, RoomId, UserId, event_id,
	events::{
		StateEventType,
		room::member::{MembershipState, RoomMemberEventContent},
	},
	room_id,
	serde::Raw,
	user_id,
};
use serde_json::json;
use tuwunel_core::{Result, config::Figment};

use crate::{
	Services,
	rooms::state_cache::MembershipUpdate,
	test_utils::{
		Fixture, fixture, membership_update, pdu_json, save_room_state, store_outlier,
		with_state_key,
	},
};

/// Withdrawing a knock on a room this server is not in tries make_leave at
/// the room's server, then clears the knock here.
///
/// Nothing listens at the room's server, so the make_leave attempt shows as
/// that server's recorded failure.
#[tokio::test]
async fn withdrawing_a_remote_knock_asks_the_room() -> Result {
	let Some(fixture) = loopback_fixture().await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!knock:127.0.0.1:2");
	let user_id = user_id!("@alice:localhost");

	pending(services, room_id, user_id, MembershipState::Knock, "@creator:127.0.0.1:2").await?;
	leave(services, room_id, user_id).await?;

	let state_cache = &services.state_cache;

	assert!(asked(services, "127.0.0.1:2").await?, "the room was not asked");
	assert!(!state_cache.is_knocked(user_id, room_id).await, "the knock remains");
	assert!(state_cache.is_left(user_id, room_id).await, "the room is not left");

	Ok(())
}

/// Rejecting an invite to a room this server is not in tries make_leave at
/// the room's server.
///
/// Room state holds no member event for the user, which sends a leave that is
/// not a knock's withdrawal through the room as well.
#[tokio::test]
async fn rejecting_a_remote_invite_asks_the_room() -> Result {
	let Some(fixture) = loopback_fixture().await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!invite:127.0.0.1:5");
	let user_id = user_id!("@alice:localhost");

	pending(services, room_id, user_id, MembershipState::Invite, "@creator:127.0.0.1:5").await?;
	leave(services, room_id, user_id).await?;

	let state_cache = &services.state_cache;

	assert!(asked(services, "127.0.0.1:5").await?, "the room was not asked");
	assert!(!state_cache.is_invited(user_id, room_id).await, "the invite remains");
	assert!(state_cache.is_left(user_id, room_id).await, "the room is not left");

	Ok(())
}

/// A knock sent after this server left the room still withdraws through the
/// room.
///
/// The room state kept from before shows the user's old leave, which a knock
/// stored only as a summary does not replace.
#[tokio::test]
async fn withdrawing_a_knock_after_this_server_left_asks_the_room() -> Result {
	let Some(fixture) = loopback_fixture().await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room_id = room_id!("!left:127.0.0.1:4");
	let user_id = user_id!("@alice:localhost");
	let state_key = user_id.as_str();
	let event_id = event_id!("$leave:localhost");
	let content = json!({ "membership": "leave" });
	let pdu = pdu_json(event_id, room_id, user_id, "m.room.member", &content);
	let pdu = with_state_key(pdu, state_key);
	let member = StateEventType::RoomMember;
	let entry = store_outlier(services, &member, state_key, event_id, pdu).await?;

	save_room_state(services, room_id, Arc::new([entry].into())).await?;

	let membership = services
		.state_accessor
		.room_state_get_content::<RoomMemberEventContent>(room_id, &member, state_key)
		.await?
		.membership;

	assert_eq!(membership, MembershipState::Leave, "room state lacks the old leave");

	pending(services, room_id, user_id, MembershipState::Knock, "@creator:127.0.0.1:4").await?;
	leave(services, room_id, user_id).await?;

	assert!(asked(services, "127.0.0.1:4").await?, "the room was not asked");

	Ok(())
}

/// Withdrawing a knock on a room this server is in stays local.
///
/// The knock's summary names a remote sender, which a remote withdrawal would
/// ask.
#[tokio::test]
async fn withdrawing_a_knock_in_a_resident_room_stays_local() -> Result {
	let Some(fixture) = loopback_fixture().await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let state_cache = &services.state_cache;
	let room_id = room_id!("!resident:localhost");
	let user_id = user_id!("@alice:localhost");
	let joined = user_id!("@bob:localhost");
	let count = services.globals.next_count();

	state_cache
		.update_membership(membership_update(room_id, joined, MembershipState::Join, *count))
		.await?;

	drop(count);

	let ours = services.globals.server_name();

	assert!(state_cache.server_in_room(ours, room_id).await, "not resident");

	pending(services, room_id, user_id, MembershipState::Knock, "@creator:127.0.0.1:3").await?;
	leave(services, room_id, user_id).await?;

	assert!(!asked(services, "127.0.0.1:3").await?, "a remote was asked");
	assert!(state_cache.is_left(user_id, room_id).await, "the room is not left");

	Ok(())
}

async fn loopback_fixture() -> Result<Option<Fixture>> {
	let config = Figment::new().merge(("network.http.ip_range_denylist", Vec::<String>::new()));

	fixture(config).await
}

/// Records a knock or an invite the way one from a remote server arrives.
///
/// Only the room's summary is stored, with no room state and no change to the
/// room's servers.
async fn pending(
	services: &Services,
	room_id: &RoomId,
	user_id: &UserId,
	membership: MembershipState,
	creator: &str,
) -> Result {
	let create = json!({
		"type": "m.room.create", "state_key": "", "sender": creator,
		"content": { "room_version": "11" },
	});

	let summary = vec![Raw::new(&create).map(Raw::cast_unchecked)?];
	let count = services.globals.next_count();

	services
		.state_cache
		.update_membership(MembershipUpdate {
			last_state: Some(summary),
			update_joined_count: false,
			..membership_update(room_id, user_id, membership, *count)
		})
		.await
}

async fn leave(services: &Services, room_id: &RoomId, user_id: &UserId) -> Result {
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.membership
		.leave(user_id, room_id, None, false, &state_lock)
		.await
}

/// Tells whether a request to `server` failed.
///
/// Nothing listens at the servers these tests name, so a recorded failure is
/// the trace a make_leave attempt leaves.
async fn asked(services: &Services, server: &str) -> Result<bool> {
	let server: OwnedServerName = server.try_into()?;

	let failed = services
		.federation
		.peer_has_failures(&server)
		.await;

	Ok(failed)
}
