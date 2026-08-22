use std::{collections::BTreeMap, iter::once};

use minicbor_serde::{from_slice, to_vec};
use ruma::{
	OwnedRoomId, RoomId, UInt,
	api::client::sync::sync_events::v5::{
		ListId, Ranges, Request,
		request::{self, List, ListConfig, ListFilters},
	},
	directory::RoomTypeFilter,
	events::StateEventType,
	room_id,
};
use serde::Serialize;

use super::{Connection, Lists, Room, Subscriptions};

const LIST_ID: &str = "main";

#[test]
fn update_cache_replaces_existing_list_ranges() {
	let mut conn = Connection::default();

	assert!(conn.update_cache(&request_with_list(list_with_ranges(&[(0, 19)]))));
	assert!(conn.update_cache(&request_with_list(list_with_ranges(&[(20, 39)]))));

	assert_cached_ranges(&conn, &[(20, 39)]);
}

#[test]
fn update_cache_allows_empty_ranges_to_replace_existing_ranges() {
	let mut conn = Connection::default();

	assert!(conn.update_cache(&request_with_list(list_with_ranges(&[(0, 19)]))));
	assert!(conn.update_cache(&request_with_list(list_with_ranges(&[]))));

	assert_cached_ranges(&conn, &[]);
}

#[test]
fn update_cache_keeps_ranges_when_list_is_omitted() {
	let mut conn = Connection::default();

	assert!(conn.update_cache(&request_with_list(list_with_ranges(&[(0, 19)]))));
	assert!(!conn.update_cache(&Request::new()));

	assert_cached_ranges(&conn, &[(0, 19)]);
}

#[test]
fn update_cache_preserves_sticky_list_metadata() {
	let mut conn = Connection::default();
	let required_state = vec![(StateEventType::RoomMember, "$LAZY".into())];

	assert!(conn.update_cache(&request_with_list(list_with_required_state(
		&[(0, 19)],
		required_state.clone(),
	))));

	assert!(conn.update_cache(&request_with_list(list_with_ranges(&[(20, 39)]))));

	let cached = conn
		.lists
		.get(&list_id())
		.expect("list must remain cached");

	assert_eq!(cached.room_details.required_state, required_state);
	assert_cached_ranges(&conn, &[(20, 39)]);
}

#[test]
fn update_cache_clears_dropped_list_filters() {
	let mut conn = Connection::default();
	let dropped = ListFilters {
		not_room_types: vec![RoomTypeFilter::Space],
		..Default::default()
	};

	assert!(conn.update_cache(&request_with_list(list_with_filters(dropped))));
	assert!(conn.update_cache(&request_with_list(list_with_filters(ListFilters::default()))));

	let filters = cached_filters(&conn);

	assert!(
		filters.not_room_types.is_empty(),
		"a filter the client dropped must clear, not persist stickily"
	);
}

#[test]
fn update_cache_keeps_filters_when_omitted() {
	let mut conn = Connection::default();
	let kept = ListFilters {
		not_room_types: vec![RoomTypeFilter::Space],
		..Default::default()
	};

	assert!(conn.update_cache(&request_with_list(list_with_filters(kept))));
	assert!(conn.update_cache(&request_with_list(list_with_ranges(&[(0, 19)]))));

	let filters = cached_filters(&conn);

	assert_eq!(filters.not_room_types, vec![RoomTypeFilter::Space]);
}

#[test]
fn epilogue_advances_only_complete_ranges() {
	let complete = room_id!("!a:example.com");
	let incomplete = room_id!("!b:example.com");
	let mut conn = Connection {
		next_batch: 5,
		rooms: [
			(complete.to_owned(), Room { roomsince: 3, config_hash: 11 }),
			(incomplete.to_owned(), Room { roomsince: 3, config_hash: 12 }),
		]
		.into(),
		..Default::default()
	};

	conn.update_rooms_epilogue(once((complete, Some(23))));

	assert_eq!(conn.rooms[complete].roomsince, 5);
	assert_eq!(conn.rooms[complete].config_hash, 23);
	assert_eq!(conn.rooms[incomplete].roomsince, 3);
	assert_eq!(conn.rooms[incomplete].config_hash, 12);
}

#[test]
fn epilogue_tracks_a_first_complete_range() {
	let complete = room_id!("!new:example.com");
	let mut conn = Connection { next_batch: 7, ..Default::default() };

	conn.update_rooms_epilogue(once((complete, None)));

	assert_eq!(conn.rooms[complete].roomsince, 7);
	assert_eq!(conn.rooms[complete].config_hash, 0);
}

#[test]
fn prologue_rewinds_a_complete_range_for_replay() {
	let replay = room_id!("!replay:example.com");
	let retained = room_id!("!retained:example.com");
	let mut conn = Connection {
		rooms: [
			(replay.to_owned(), Room { roomsince: 9, config_hash: 17 }),
			(retained.to_owned(), Room { roomsince: 4, config_hash: 18 }),
		]
		.into(),
		..Default::default()
	};

	conn.update_rooms_prologue(Some(5));

	assert_eq!(conn.rooms[replay].roomsince, 5);
	assert_eq!(conn.rooms[replay].config_hash, 0);
	assert_eq!(conn.rooms[retained].roomsince, 4);
	assert_eq!(conn.rooms[retained].config_hash, 18);
}

#[test]
fn update_cache_identical_effective_list_is_clean() {
	let mut conn = Connection::default();
	let list = List {
		ranges: ranges_from_u64(&[(0, 19)]),
		room_details: ListConfig {
			required_state: vec![(StateEventType::RoomMember, "$LAZY".into())],
			timeline_limit: uint(1),
		},
		filters: Some(ListFilters {
			not_room_types: vec![RoomTypeFilter::Space],
			..Default::default()
		}),
	};

	assert!(conn.update_cache(&request_with_list(list.clone())));
	assert!(!conn.update_cache(&request_with_list(list)));
}

#[test]
fn update_cache_sticky_omissions_are_clean() {
	let mut conn = Connection::default();
	let list = List {
		ranges: ranges_from_u64(&[(0, 19)]),
		room_details: ListConfig {
			required_state: vec![(StateEventType::RoomMember, "$LAZY".into())],
			..Default::default()
		},
		filters: Some(ListFilters {
			not_room_types: vec![RoomTypeFilter::Space],
			..Default::default()
		}),
	};

	assert!(conn.update_cache(&request_with_list(list)));
	assert!(!conn.update_cache(&request_with_list(list_with_ranges(&[(0, 19)]))));
}

#[test]
fn update_cache_copies_changed_timeline_limit() {
	let mut conn = Connection::default();

	assert!(conn.update_cache(&request_with_list(list_with_timeline_limit(1))));
	assert!(conn.update_cache(&request_with_list(list_with_timeline_limit(2))));

	let cached = conn
		.lists
		.get(&list_id())
		.expect("list must be cached");

	assert_eq!(cached.room_details.timeline_limit, uint(2));

	assert!(conn.update_cache(&request_with_list(list_with_timeline_limit(0))));

	let cached = conn
		.lists
		.get(&list_id())
		.expect("list must be cached");

	assert_eq!(cached.room_details.timeline_limit, uint(0));
}

#[test]
fn update_cache_detects_changed_required_state() {
	let mut conn = Connection::default();
	let room_name = vec![(StateEventType::RoomName, "".into())];
	let room_member = vec![(StateEventType::RoomMember, "$LAZY".into())];
	let initial = list_with_required_state(&[(0, 19)], room_name);
	let changed = list_with_required_state(&[(0, 19)], room_member.clone());

	assert!(conn.update_cache(&request_with_list(initial)));
	assert!(conn.update_cache(&request_with_list(changed)));

	let cached = conn
		.lists
		.get(&list_id())
		.expect("list must be cached");

	assert_eq!(cached.room_details.required_state, room_member);
}

#[test]
fn update_cache_detects_new_default_list() {
	let mut conn = Connection::default();

	assert!(conn.update_cache(&request_with_list(List::default())));
}

#[test]
fn update_cache_tracks_subscription_changes() {
	let room_id = room_id!("!subscription:example.com");
	let initial = ListConfig {
		required_state: vec![(StateEventType::RoomName, "".into())],
		..Default::default()
	};

	let expanded = ListConfig {
		required_state: vec![
			(StateEventType::RoomName, "".into()),
			(StateEventType::RoomMember, "$LAZY".into()),
		],
		..Default::default()
	};

	let mut conn = Connection::default();

	assert!(conn.update_cache(&request_with_subscription(room_id, initial.clone())));
	assert!(!conn.update_cache(&request_with_subscription(room_id, initial)));
	assert!(conn.update_cache(&request_with_subscription(room_id, expanded.clone())));
	assert!(!conn.update_cache(&request_with_subscription(room_id, expanded)));
	assert!(conn.update_cache(&Request::new()));
	assert!(!conn.update_cache(&Request::new()));
}

#[test]
fn epilogue_leaves_hash_for_extension_only_range() {
	let room_id = room_id!("!extension:example.com");
	let mut conn = Connection {
		next_batch: 7,
		rooms: [(room_id.to_owned(), Room { roomsince: 3, config_hash: 19 })].into(),
		..Default::default()
	};

	conn.update_rooms_epilogue(once((room_id, None)));

	assert_eq!(conn.rooms[room_id].roomsince, 7);
	assert_eq!(conn.rooms[room_id].config_hash, 19);
}

#[test]
fn old_connection_cbor_defaults_room_hash() {
	#[derive(Serialize)]
	struct RoomV0 {
		roomsince: u64,
	}

	#[derive(Serialize)]
	struct ConnectionV0 {
		globalsince: u64,
		next_batch: u64,
		lists: Lists,
		extensions: request::Extensions,
		subscriptions: Subscriptions,
		rooms: BTreeMap<OwnedRoomId, RoomV0>,
	}

	let room_id = room_id!("!legacy:example.com");
	let legacy = ConnectionV0 {
		globalsince: 5,
		next_batch: 8,
		lists: Default::default(),
		extensions: Default::default(),
		subscriptions: Default::default(),
		rooms: [(room_id.to_owned(), RoomV0 { roomsince: 7 })].into(),
	};

	let bytes = to_vec(&legacy).expect("old connection must encode");
	let decoded: Connection = from_slice(&bytes).expect("old connection must decode");

	assert_eq!(decoded.globalsince, 5);
	assert_eq!(decoded.next_batch, 8);
	assert_eq!(decoded.rooms[room_id].roomsince, 7);
	assert_eq!(decoded.rooms[room_id].config_hash, 0);
}

fn request_with_list(list: List) -> Request {
	let mut request = Request::new();

	request.lists.insert(list_id(), list);

	request
}

fn request_with_subscription(room_id: &RoomId, config: ListConfig) -> Request {
	let mut request = Request::new();

	request.room_subscriptions = [(room_id.to_owned(), config)].into();

	request
}

fn list_with_ranges(ranges: &[(u64, u64)]) -> List {
	list_with_required_state(ranges, Vec::new())
}

fn list_with_timeline_limit(timeline_limit: u64) -> List {
	List {
		room_details: ListConfig {
			timeline_limit: uint(timeline_limit),
			..Default::default()
		},
		..Default::default()
	}
}

fn list_with_required_state(
	ranges: &[(u64, u64)],
	required_state: Vec<(StateEventType, ruma::events::StateKey)>,
) -> List {
	List {
		ranges: ranges_from_u64(ranges),
		room_details: ListConfig { required_state, ..Default::default() },
		..Default::default()
	}
}

fn list_with_filters(filters: ListFilters) -> List {
	List {
		filters: Some(filters),
		..Default::default()
	}
}

fn cached_filters(conn: &Connection) -> ListFilters {
	conn.lists
		.get(&list_id())
		.expect("list must be cached")
		.filters
		.clone()
		.expect("filters must be cached")
}

fn assert_cached_ranges(conn: &Connection, expected: &[(u64, u64)]) {
	let cached = conn
		.lists
		.get(&list_id())
		.expect("list must be cached");

	assert_eq!(cached.ranges, ranges_from_u64(expected));
}

fn ranges_from_u64(ranges: &[(u64, u64)]) -> Ranges {
	ranges
		.iter()
		.map(|&(start, end)| (uint(start), uint(end)))
		.collect()
}

fn uint(value: u64) -> UInt { UInt::new(value).expect("range value must fit UInt") }

fn list_id() -> ListId { LIST_ID.into() }
