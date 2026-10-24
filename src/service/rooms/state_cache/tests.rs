use futures::StreamExt;
use ruma::{OwnedRoomId, room_id, user_id};
use serde_json::json;
use tuwunel_core::{Result, config::Figment};
use tuwunel_database::Json;

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
