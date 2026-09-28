use std::{thread::scope, time::Instant};

use ruma::{MilliSecondsSinceUnixEpoch, RoomVersionId, event_id, room_id, server_name, uint};

use super::{InFlightWalks, PrevUpgrade, Walk};

#[test]
fn poisoned_registry_keeps_listing() {
	let registry = InFlightWalks::default();
	let event_id = event_id!("$incoming");
	let started = Instant::now();
	let upgrade = PrevUpgrade {
		origin: server_name!("origin.test.local"),
		room_id: room_id!("!room:origin.test.local"),
		event_id,
		room_version: &RoomVersionId::V11,
		recursion_level: 0,
		first_ts_in_room: MilliSecondsSinceUnixEpoch(uint!(0)),
		create_event_id: event_id!("$create"),
	};

	let id = registry.insert(&upgrade, started);

	scope(|threads| {
		let poisoner = threads.spawn(|| {
			let _held = registry.lock();

			panic!("poisoning the registry lock");
		});

		assert!(poisoner.join().is_err(), "the poisoning thread did not panic");
	});

	assert!(registry.walks.is_poisoned(), "the registry lock was not poisoned");

	let walk = Walk {
		fetched: started,
		prevs: 2,
		capped: false,
	};

	registry.walking(id, walk);

	let listed: Vec<_> = registry
		.snapshot()
		.map(|entry| (entry.event_id, entry.walk.map(|walk| walk.prevs)))
		.collect();

	assert_eq!(listed, [(event_id.to_owned(), Some(2))], "the poisoned registry lost the walk");
	assert_eq!(registry.len(), 1, "the poisoned registry miscounted its passes");

	registry.remove(id);

	assert_eq!(registry.len(), 0, "the poisoned registry kept a removed pass");
}
