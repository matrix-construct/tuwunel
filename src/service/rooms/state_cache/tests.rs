use futures::StreamExt;
use ruma::{
	OwnedRoomId,
	events::room::member::{MembershipState, RoomMemberEventContent},
	room_id,
	serde::Raw,
	user_id,
};
use serde_json::{Value as JsonValue, json};
use tuwunel_core::{Result, config::Figment};
use tuwunel_database::Json;
use tuwunel_matrix::PduCount;

use super::MembershipUpdate;
use crate::test_utils::{fixture, json_events};

/// A user's room lists hold only the rows keyed by that exact user ID, not
/// those of a user whose server name extends hers.
#[tokio::test]
async fn room_lists_stop_at_the_user_id_boundary() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let alice = user_id!("@alice:example.test");
	let neighbor = user_id!("@alice:example.test.extra");
	let own = room_id!("!own:example.test");
	let other = room_id!("!other:example.test.extra");

	for map in [
		"userroomid_joined",
		"userroomid_invitestate",
		"userroomid_knockedstate",
		"userroomid_leftstate",
	] {
		services.db[map].put_raw((alice, own), b"");
		services.db[map].put_raw((neighbor, other), b"");
	}

	let state_cache = &services.state_cache;
	// heterogeneous stream array
	let lists = [
		state_cache.rooms_joined(alice).boxed(),
		state_cache.rooms_invited(alice).boxed(),
		state_cache.rooms_knocked(alice).boxed(),
		state_cache.rooms_left(alice).boxed(),
	];

	for rooms in lists {
		let rooms: Vec<OwnedRoomId> = rooms.map(ToOwned::to_owned).collect().await;

		assert_eq!(rooms, [own.to_owned()]);
	}

	Ok(())
}

/// A replayed knock carrying no stripped state keeps the summary stored with
/// the knock.
///
/// A remote join by another local user replays the knock with no stripped
/// state. A replay carrying an empty list keeps the stored summary too.
#[tokio::test]
async fn a_replayed_knock_keeps_its_stored_summary() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let state_cache = &fixture.services.state_cache;
	let alice = user_id!("@alice:localhost");
	let room_id = room_id!("!knock:example.test");
	let name_event = json!({
		"type": "m.room.name", "state_key": "", "sender": "@bob:example.test",
		"content": {"name": "Knock"},
	});

	let summary = vec![Raw::new(&name_event)?.cast_unchecked()];
	let knock = |count, last_state| MembershipUpdate {
		room_id,
		user_id: alice,
		membership_event: RoomMemberEventContent::new(MembershipState::Knock),
		sender: alice,
		last_state,
		invite_via: None,
		update_joined_count: false,
		count: PduCount::Normal(count),
	};

	state_cache
		.update_membership(knock(1, Some(summary)))
		.await?;

	for replay in [knock(2, None), knock(3, Some(Vec::new()))] {
		state_cache.update_membership(replay).await?;
	}

	let stored = state_cache.knock_state(alice, room_id).await?;

	assert_eq!(stored.len(), 1, "the stored summary survives the replays");

	let stored_event: JsonValue = stored[0].deserialize_as_unchecked()?;

	assert_eq!(stored_event, name_event);

	Ok(())
}

/// A knock summary is read with only the last of the user's own member events.
///
/// An earlier version stored a re-knock's summary with the user's previous
/// leave ahead of the knock, and a client reading the first member event about
/// itself would take the room for an invite.
#[tokio::test]
async fn a_knock_summary_reads_only_the_latest_own_membership() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let alice = user_id!("@alice:localhost");
	let room_id = room_id!("!knock:example.test");
	let member = |membership| {
		json!({
			"type": "m.room.member", "state_key": alice, "sender": alice,
			"content": {"membership": membership},
		})
	};

	let name_event = json!({
		"type": "m.room.name", "state_key": "", "sender": "@bob:example.test",
		"content": {"name": "Knock"},
	});

	let stored = json!([member("leave"), name_event, member("knock")]);

	services.db["userroomid_knockedstate"].put((alice, room_id), Json(&stored));

	let state_cache = &services.state_cache;
	let read = state_cache.knock_state(alice, room_id).await?;
	let listed: Vec<_> = state_cache
		.rooms_knocked_state(alice)
		.map(|(_, state)| state)
		.collect()
		.await;

	assert_eq!(json_events(&read)?, [name_event.clone(), member("knock")]);
	assert_eq!(listed.len(), 1);
	assert_eq!(json_events(&listed[0])?, [name_event, member("knock")]);

	Ok(())
}
