use futures::StreamExt;
use ruma::{OwnedRoomId, room_id, user_id};
use tuwunel_core::{Result, config::Figment};

use crate::test_utils::fixture;

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
