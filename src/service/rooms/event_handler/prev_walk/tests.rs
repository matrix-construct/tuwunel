use std::{
	iter::from_fn,
	thread::scope,
	time::{Duration, Instant},
};

use futures::{FutureExt, StreamExt};
use ruma::{
	EventId, MilliSecondsSinceUnixEpoch, RoomId, RoomVersionId, event_id, owned_event_id,
	owned_server_name, room_id, server_name, uint,
};
use tuwunel_core::utils::{stream::IterStream, time::timepoint_from_epoch};
use tuwunel_database::{Interfix, deserialize_from_slice, serialize_key, serialize_val};

use super::{
	InFlightWalks, Outcome, Pass, PrevUpgrade, PrevWalkPass, PrevWalkRoom, Walk,
	history::{PassKey, PassRow, PassVal, pass_row, room_passes, tally},
};

const OUTCOMES: [Outcome; 8] = [
	Outcome::Held,
	Outcome::Closed,
	Outcome::FetchFailed,
	Outcome::FetchCancelled,
	Outcome::Appended,
	Outcome::NotAppended,
	Outcome::Failed,
	Outcome::Cancelled,
];

#[test]
fn poisoned_registry_keeps_listing() {
	let registry = InFlightWalks::default();
	let room_version = RoomVersionId::V11;
	let upgrade = prev_upgrade(&room_version);
	let started = Instant::now();
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

	assert_eq!(
		listed,
		[(upgrade.event_id.to_owned(), Some(2))],
		"the poisoned registry lost the walk"
	);

	assert_eq!(registry.len(), 1, "the poisoned registry miscounted its passes");

	registry.remove(id);

	assert_eq!(registry.len(), 0, "the poisoned registry kept a removed pass");
}

#[test]
fn pass_splits_its_time_at_the_walk() {
	let started = Instant::now();
	let at = |secs| {
		started
			.checked_add(Duration::from_secs(secs))
			.expect("the test instant is in range")
	};

	let walk = Walk { fetched: at(2), prevs: 4, capped: true };
	let ended = at(5);
	let passes = [
		Pass::new(started, Some(walk), Some((Outcome::NotAppended, 1)), ended),
		Pass::new(started, None, Some((Outcome::FetchFailed, 0)), ended),
		Pass::new(started, Some(walk), None, ended),
		Pass::new(started, None, None, ended),
	];

	let ends = passes.map(|pass| {
		let Pass {
			outcome,
			prevs,
			unprocessed,
			capped,
			fetch,
			upgrade,
		} = pass;

		(outcome.name(), prevs, unprocessed, capped, fetch.as_secs(), upgrade.as_secs())
	});

	let expected = [
		("not_appended", 4, 1, true, 2, 3),
		("fetch_failed", 0, 0, false, 5, 0),
		("cancelled", 4, 0, true, 2, 3),
		("fetch_cancelled", 0, 0, false, 5, 0),
	];

	assert_eq!(ends, expected, "a pass split its time or mapped its outcome wrong");
}

#[test]
fn pass_row_round_trips() {
	let room_version = RoomVersionId::V11;
	let upgrade = prev_upgrade(&room_version);
	let pass = Pass {
		outcome: Outcome::Cancelled,
		prevs: 3,
		unprocessed: 2,
		capped: true,
		fetch: Duration::from_micros(850_900),
		upgrade: Duration::from_millis(1_200),
	};

	let expected_key =
		b"!room:origin.test.local\xFF\x01\x02\x03\x04\x05\x06\x07\x08\xFF$incoming";

	let expected_val = b"\x07\xFF\
		\0\0\0\0\0\0\0\x03\xFF\
		\0\0\0\0\0\0\0\x02\xFF\
		\x01\xFF\
		\0\0\0\0\0\0\x03\x52\xFF\
		\0\0\0\0\0\0\x04\xB0\xFF\
		origin.test.local";

	let (key, val) = pass_row(&upgrade, &pass, 0x0102_0304_0506_0708);
	let key_bytes = serialize_key(key).expect("the pass key serializes");
	let val_bytes = serialize_val(val).expect("the pass value serializes");

	assert_eq!(key_bytes.as_slice(), expected_key, "the pass key is not room, end, then event");
	assert_eq!(val_bytes.as_slice(), expected_val, "the pass value is not in field order");
	assert_eq!(val_bytes.len(), 40 + "origin.test.local".len(), "the pass value grew");

	let decoded_key: PassKey<'_> =
		deserialize_from_slice(&key_bytes).expect("the pass key decodes");

	let decoded_val: PassVal<'_> =
		deserialize_from_slice(&val_bytes).expect("the pass value decodes");

	assert_eq!(decoded_key, key, "the pass key did not round-trip");
	assert_eq!(decoded_val, val, "the pass value did not round-trip");
}

#[test]
fn pass_keys_order_by_room_then_time() {
	let room = room_id!("!room:origin.test.local");
	let extended = room_id!("!room:origin.test.local.extended");
	let key = |room_id: &RoomId, ended_ms: u64, event_id: &EventId| {
		serialize_key((room_id, ended_ms, event_id)).expect("the pass key serializes")
	};

	let earlier = key(room, 1, event_id!("$b"));
	let later = key(room, 2, event_id!("$a"));
	let extension = key(extended, u64::MAX, event_id!("$c"));
	let bound = serialize_key((room, u64::MAX, Interfix)).expect("the read bound serializes");

	assert!(earlier < later, "a room's passes are not in end-time order");
	assert!(extension < earlier, "a room extending the id does not sort below it");
	assert!(later < bound, "the latest-first read starts below a pass");
}

#[test]
fn outcome_codes_are_pinned() {
	let codes = OUTCOMES.map(u8::from);

	assert_eq!(codes, [0, 1, 2, 3, 4, 5, 6, 7], "an outcome changed its recorded code");
	assert_eq!(codes.map(Outcome::try_from), OUTCOMES.map(Ok), "a code decoded wrong");
	assert_eq!(Outcome::try_from(8), Err(8), "an unknown code decoded to an outcome");
}

#[test]
fn rows_fold_per_room() {
	let room = room_id!("!room:origin.test.local");
	let extended = room_id!("!room:origin.test.local.extended");
	let rows: [PassRow<'_>; 3] = [
		((extended, 5, event_id!("$c")), (200, 1, 1, 0, 2, 0, "origin.test.local")),
		((room, 6, event_id!("$a")), (4, 3, 1, 0, 40, 500, "origin.test.local")),
		((room, 7, event_id!("$b")), (7, 2, 0, 1, 60, 1_500, "origin.test.local")),
	];

	let rooms = rows.into_iter().fold(Vec::new(), tally);
	let expected = [
		PrevWalkRoom {
			passes: 1,
			prevs: 1,
			unprocessed: 1,
			fetch: Duration::from_millis(2),
			..PrevWalkRoom::empty(extended)
		},
		PrevWalkRoom {
			passes: 2,
			appended: 1,
			cancelled: 1,
			capped: 1,
			prevs: 5,
			unprocessed: 1,
			fetch: Duration::from_millis(100),
			upgrade: Duration::from_secs(2),
			..PrevWalkRoom::empty(room)
		},
	];

	assert_eq!(rooms, expected, "the rows did not fold into one total per room");
}

#[test]
fn room_read_stops_at_the_boundary() {
	let room = room_id!("!room:origin.test.local");
	let extended = room_id!("!room:origin.test.local.extended");
	let rows: [PassRow<'_>; 3] = [
		((room, 1_500, event_id!("$b")), (3, 0, 0, 0, 7, 0, "origin.test.local")),
		((room, 1_000, event_id!("$a")), (201, 2, 1, 1, 9, 11, "not a server name")),
		((extended, 2_000, event_id!("$c")), (4, 1, 0, 0, 1, 1, "origin.test.local")),
	];

	let rows = rows
		.into_iter()
		.chain(from_fn(|| panic!("the read went past the room")));

	let passes: Vec<_> = room_passes(rows.stream(), room)
		.collect()
		.now_or_never()
		.expect("the rows are ready");

	let ended = |millis| {
		timepoint_from_epoch(Duration::from_millis(millis)).expect("the test time is in range")
	};

	let expected = [
		PrevWalkPass {
			ended: ended(1_500),
			event_id: owned_event_id!("$b"),
			origin: Some(owned_server_name!("origin.test.local")),
			outcome: Some(Outcome::FetchCancelled),
			prevs: 0,
			unprocessed: 0,
			capped: false,
			fetch: Duration::from_millis(7),
			upgrade: Duration::ZERO,
		},
		PrevWalkPass {
			ended: ended(1_000),
			event_id: owned_event_id!("$a"),
			origin: None,
			outcome: None,
			prevs: 2,
			unprocessed: 1,
			capped: true,
			fetch: Duration::from_millis(9),
			upgrade: Duration::from_millis(11),
		},
	];

	assert_eq!(passes, expected, "the room read did not stop at the next room");
}

fn prev_upgrade(room_version: &RoomVersionId) -> PrevUpgrade<'_> {
	PrevUpgrade {
		origin: server_name!("origin.test.local"),
		room_id: room_id!("!room:origin.test.local"),
		event_id: event_id!("$incoming"),
		room_version,
		recursion_level: 0,
		first_ts_in_room: MilliSecondsSinceUnixEpoch(uint!(0)),
		create_event_id: event_id!("$create"),
	}
}
