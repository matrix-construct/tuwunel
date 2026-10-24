use std::{
	collections::BTreeMap,
	iter::{Once, once},
};

use minicbor_serde::{from_slice, to_vec};
use ruma::{
	OwnedRoomId, RoomId, UInt,
	api::client::sync::sync_events::v5::{
		ListId, Ranges, Request,
		request::{self, List, ListConfig, ListFilters},
	},
	directory::RoomTypeFilter,
	events::StateEventType,
	profile::ProfileFieldName,
	room_id,
};
use serde::{Deserialize, Serialize};
use tuwunel_core::Err;

use super::{
	Connection, Lists, RequiredState, Room, RoomUpdate, Subscriptions, UnreadState,
	confirmed_removed,
};

const LIST_ID: &str = "main";

/// A room as stored since v1.9.2, before rooms recorded a delivered snapshot.
///
/// `room_cbor_defaults_snapshot_and_allows_downgrade` checks the encoder still writes it.
const ROOM_V2: &[u8] =
	b"\xA3\x69roomsince\x07\x6Bconfig_hash\x13\x6Erequired_state\x84\x01\x02\x03\x04";

#[derive(Deserialize, Serialize)]
struct RoomV1 {
	roomsince: u64,
	config_hash: u64,
}

#[derive(Deserialize, Serialize)]
struct RoomV2 {
	roomsince: u64,
	config_hash: u64,
	required_state: RequiredState,
}

/// A room as stored before rooms recorded the keys a delivery could not read.
///
/// `room_cbor_writes_unread_state_only_after_a_failed_read` checks a room with
/// no unread keys still encodes to this shape.
#[derive(Deserialize, Serialize)]
struct RoomV3 {
	roomsince: u64,
	config_hash: u64,
	required_state: RequiredState,
	shortstatehash: u64,
}

#[derive(Deserialize, Serialize)]
struct ConnectionV1 {
	globalsince: u64,
	next_batch: u64,
	lists: Lists,
	extensions: request::Extensions,
	subscriptions: Subscriptions,
	rooms: BTreeMap<OwnedRoomId, RoomV1>,
}

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
	let complete_room = Room {
		roomsince: 3,
		config_hash: 11,
		required_state: [2, 4].into_iter().collect(),
		shortstatehash: 29,
		..Default::default()
	};

	let incomplete_room = Room {
		roomsince: 3,
		config_hash: 12,
		required_state: [3, 5].into_iter().collect(),
		shortstatehash: 29,
		..Default::default()
	};

	let mut conn = Connection {
		next_batch: 5,
		rooms: [(complete.to_owned(), complete_room), (incomplete.to_owned(), incomplete_room)]
			.into(),
		..Default::default()
	};

	conn.update_rooms_epilogue(3, delivery(complete, (23, &[4], 31)));

	assert_eq!(conn.rooms[complete].roomsince, 5);
	assert_eq!(conn.rooms[complete].config_hash, 23);
	assert_eq!(conn.rooms[complete].required_state.as_slice(), &[4]);
	assert_eq!(conn.rooms[complete].shortstatehash, 31);
	assert_eq!(conn.rooms[incomplete].roomsince, 3);
	assert_eq!(conn.rooms[incomplete].config_hash, 12);
	assert_eq!(conn.rooms[incomplete].required_state.as_slice(), &[3, 5]);
	assert_eq!(conn.rooms[incomplete].shortstatehash, 29);
}

#[test]
fn epilogue_tracks_a_first_complete_range() {
	let complete = room_id!("!new:example.com");
	let mut conn = Connection { next_batch: 7, ..Default::default() };

	conn.update_rooms_epilogue(3, once((complete, None)));

	assert_eq!(conn.rooms[complete].roomsince, 7);
	assert_eq!(conn.rooms[complete].config_hash, 0);
	assert!(conn.rooms[complete].required_state.is_empty());
}

#[test]
fn prologue_rewinds_a_complete_range_for_replay() {
	let replay = room_id!("!replay:example.com");
	let retained = room_id!("!retained:example.com");
	let replay_room = Room {
		roomsince: 9,
		config_hash: 17,
		required_state: [2, 4].into_iter().collect(),
		shortstatehash: 29,
		..Default::default()
	};

	let retained_room = Room {
		roomsince: 4,
		config_hash: 18,
		required_state: [3, 5].into_iter().collect(),
		shortstatehash: 31,
		..Default::default()
	};

	let mut conn = Connection {
		rooms: [(replay.to_owned(), replay_room), (retained.to_owned(), retained_room)].into(),
		..Default::default()
	};

	conn.update_rooms_prologue(Some(5));

	assert_eq!(conn.rooms[replay].roomsince, 5);
	assert_eq!(conn.rooms[replay].config_hash, 0);
	assert!(conn.rooms[replay].required_state.is_empty());
	assert_eq!(conn.rooms[replay].shortstatehash, 0);
	assert_eq!(conn.rooms[retained].roomsince, 4);
	assert_eq!(conn.rooms[retained].config_hash, 18);
	assert_eq!(conn.rooms[retained].required_state.as_slice(), &[3, 5]);
	assert_eq!(conn.rooms[retained].shortstatehash, 31);
}

#[test]
fn retry_restores_the_room_as_the_lost_request_found_it() {
	let room_id = room_id!("!retried:example.com");
	let mut conn = connection_with_room(room_id);

	conn.update_rooms_epilogue(5, delivery(room_id, (23, &[4, 6], 31)));
	conn.next_batch = 12;
	conn.update_rooms_epilogue(5, delivery(room_id, (37, &[6], 41)));
	conn.update_rooms_prologue(Some(5));

	let room = &conn.rooms[room_id];

	assert_delivery(room, 5, (19, &[4], 29));
	assert!(room.checkpoint.is_none());
}

#[test]
fn retry_restores_the_keys_the_lost_request_read() {
	let room_id = room_id!("!unread:example.com");
	let room = Room {
		unread_state: once(7).collect(),
		..delivered_room()
	};

	let mut conn = connection_with(room_id, room);

	conn.update_rooms_epilogue(5, delivery(room_id, (19, &[4], 29)));

	assert!(conn.rooms[room_id].unread_state.is_empty());

	conn.update_rooms_prologue(Some(5));

	let room = &conn.rooms[room_id];

	assert_delivery(room, 5, (19, &[4], 29));
	assert_eq!(room.unread_state.as_slice(), &[7]);
}

#[test]
fn checkpoint_holds_no_copy_of_an_unchanged_configuration() {
	let room_id = room_id!("!unchanged:example.com");
	let mut conn = connection_with_room(room_id);

	conn.update_rooms_epilogue(5, delivery(room_id, (19, &[4], 29)));

	let checkpoint = conn.rooms[room_id].checkpoint.as_ref();

	assert!(checkpoint.is_some_and(|checkpoint| checkpoint.config.is_none()));

	conn.update_rooms_prologue(Some(5));

	assert_delivery(&conn.rooms[room_id], 5, (19, &[4], 29));
}

#[test]
fn retry_restores_the_cursor_of_a_room_without_payload() {
	let room_id = room_id!("!quiet:example.com");
	let mut conn = connection_with_room(room_id);

	conn.update_rooms_epilogue(5, once((room_id, None)));
	conn.update_rooms_prologue(Some(5));

	assert_delivery(&conn.rooms[room_id], 5, (19, &[4], 29));
}

#[test]
fn retry_returns_a_room_first_delivered_by_the_lost_request_to_initial() {
	let room_id = room_id!("!first:example.com");
	let mut conn = Connection { next_batch: 9, ..Default::default() };

	conn.update_rooms_epilogue(5, delivery(room_id, (23, &[4], 31)));
	conn.update_rooms_prologue(Some(5));

	assert_delivery(&conn.rooms[room_id], 0, (0, &[], 0));
}

#[test]
fn replay_of_an_older_position_forgets_the_configuration() {
	let room_id = room_id!("!older:example.com");
	let mut conn = connection_with_room(room_id);

	conn.update_rooms_epilogue(5, once((room_id, None)));
	conn.next_batch = 12;
	conn.update_rooms_epilogue(9, once((room_id, None)));
	conn.update_rooms_prologue(Some(5));

	let room = &conn.rooms[room_id];

	assert_delivery(room, 5, (0, &[], 0));
	assert!(room.checkpoint.is_none());
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
fn epilogue_leaves_configuration_for_extension_only_range() {
	let room_id = room_id!("!extension:example.com");
	let room = Room {
		roomsince: 3,
		config_hash: 19,
		required_state: [2, 4].into_iter().collect(),
		shortstatehash: 29,
		..Default::default()
	};

	let mut conn = Connection {
		next_batch: 7,
		rooms: [(room_id.to_owned(), room)].into(),
		..Default::default()
	};

	conn.update_rooms_epilogue(3, once((room_id, None)));

	assert_eq!(conn.rooms[room_id].roomsince, 7);
	assert_eq!(conn.rooms[room_id].config_hash, 19);
	assert_eq!(conn.rooms[room_id].required_state.as_slice(), &[2, 4]);
	assert_eq!(conn.rooms[room_id].shortstatehash, 29);
}

#[test]
fn own_profile_is_owed_to_a_new_connection() {
	let mut conn = Connection::default();

	conn.update_cache(&request_with_profiles(true));

	assert!(conn.own_profile_owed());
}

#[test]
fn own_profile_is_not_owed_without_the_extension() {
	let mut conn = Connection { next_batch: 7, ..Default::default() };

	conn.update_profiles_epilogue();

	assert!(!conn.own_profile_owed());
	assert_eq!(conn.own_profile_since, 0);
}

#[test]
fn epilogue_records_an_owed_own_profile() {
	let mut conn = Connection {
		globalsince: 3,
		next_batch: 7,
		..Default::default()
	};

	conn.update_cache(&request_with_profiles(true));
	conn.update_profiles_epilogue();

	assert_eq!(conn.own_profile_since, 7);
}

#[test]
fn acknowledged_own_profile_is_not_owed() {
	let mut conn = Connection {
		globalsince: 7,
		next_batch: 9,
		own_profile_since: 7,
		..Default::default()
	};

	conn.update_cache(&request_with_profiles(true));
	conn.update_profiles_epilogue();

	assert!(!conn.own_profile_owed());
	assert_eq!(conn.own_profile_since, 7);
}

#[test]
fn own_profile_is_owed_again_on_replay() {
	let mut conn = Connection {
		globalsince: 5,
		own_profile_since: 7,
		..Default::default()
	};

	conn.update_cache(&request_with_profiles(true));

	assert!(conn.own_profile_owed());
}

#[test]
fn switching_profiles_off_owes_the_own_profile_again() {
	let mut conn = Connection {
		globalsince: 7,
		own_profile_since: 7,
		..Default::default()
	};

	conn.update_cache(&request_with_profiles(false));

	assert!(!conn.own_profile_owed());
	assert_eq!(conn.own_profile_since, 0);

	conn.update_cache(&request_with_profiles(true));

	assert!(conn.own_profile_owed());
}

#[test]
fn omitted_profiles_config_keeps_the_acknowledged_own_profile() {
	let mut conn = Connection {
		globalsince: 7,
		own_profile_since: 7,
		..Default::default()
	};

	conn.update_cache(&request_with_profiles(true));
	conn.update_cache(&Request::new());

	assert!(!conn.own_profile_owed());
	assert_eq!(conn.own_profile_since, 7);
}

#[test]
fn widening_the_fields_owes_the_own_profile_again() {
	let mut conn = Connection {
		globalsince: 7,
		own_profile_since: 7,
		..Default::default()
	};

	conn.update_cache(&request_with_fields(&[ProfileFieldName::AvatarUrl]));

	assert!(!conn.own_profile_owed());

	conn.update_cache(&request_with_fields(&[
		ProfileFieldName::AvatarUrl,
		ProfileFieldName::DisplayName,
	]));

	assert!(conn.own_profile_owed());
	assert!(conn.profiles_fields_owed());
}

#[test]
fn narrowing_the_fields_owes_nothing() {
	let mut conn = Connection {
		globalsince: 7,
		own_profile_since: 7,
		..Default::default()
	};

	conn.update_cache(&request_with_fields(&[
		ProfileFieldName::AvatarUrl,
		ProfileFieldName::DisplayName,
	]));

	conn.update_cache(&request_with_fields(&[ProfileFieldName::AvatarUrl]));

	assert!(!conn.own_profile_owed());
	assert!(!conn.profiles_fields_owed());
}

#[test]
fn retained_profile_fields_are_normalized_without_widening() {
	let mut conn = Connection {
		globalsince: 7,
		own_profile_since: 7,
		..Default::default()
	};

	let fields = [
		ProfileFieldName::DisplayName,
		ProfileFieldName::AvatarUrl,
		ProfileFieldName::DisplayName,
	];

	conn.update_cache(&request_with_fields(&fields));
	conn.update_cache(&request_with_fields(&[
		ProfileFieldName::AvatarUrl,
		ProfileFieldName::DisplayName,
	]));

	assert_eq!(
		conn.extensions
			.profiles
			.fields
			.as_ref()
			.unwrap()
			.len(),
		2
	);

	assert!(!conn.profiles_fields_owed());

	let bytes = to_vec(&conn).expect("normalized connection must encode");
	let restored: Connection = from_slice(&bytes).expect("normalized connection must decode");

	assert_eq!(restored.extensions.profiles.fields, conn.extensions.profiles.fields);

	conn.update_cache(&Request::new());
	assert_eq!(
		conn.extensions
			.profiles
			.fields
			.as_ref()
			.unwrap()
			.len(),
		2
	);

	conn.update_cache(&request_with_fields(&[]));
	assert_eq!(
		conn.extensions
			.profiles
			.fields
			.as_ref()
			.unwrap()
			.as_slice(),
		[]
	);
}

#[test]
fn an_unfiltered_connection_cannot_widen() {
	let mut conn = Connection {
		globalsince: 7,
		own_profile_since: 7,
		..Default::default()
	};

	conn.update_cache(&request_with_profiles(true));
	conn.update_cache(&request_with_fields(&[ProfileFieldName::AvatarUrl]));

	assert!(!conn.own_profile_owed());
	assert!(!conn.profiles_fields_owed());
}

#[test]
fn widened_fields_clear_once_acknowledged() {
	let mut conn = Connection {
		globalsince: 7,
		next_batch: 9,
		own_profile_since: 7,
		..Default::default()
	};

	let widened = [ProfileFieldName::AvatarUrl, ProfileFieldName::DisplayName];

	conn.update_cache(&request_with_fields(&[ProfileFieldName::AvatarUrl]));
	conn.update_cache(&request_with_fields(&widened));
	conn.update_profiles_epilogue();

	assert_eq!(conn.own_profile_since, 9);
	assert!(conn.profiles_fields_owed());

	conn.globalsince = 9;
	conn.update_cache(&request_with_fields(&widened));

	assert!(!conn.profiles_fields_owed());
	assert!(!conn.profiles_fields_widened);
}

#[test]
fn connection_cbor_is_compatible_across_versions() {
	#[derive(Deserialize, Serialize)]
	struct RoomV0 {
		roomsince: u64,
	}

	#[derive(Deserialize, Serialize)]
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
	assert!(decoded.rooms[room_id].required_state.is_empty());
	assert_eq!(decoded.rooms[room_id].shortstatehash, 0);
	assert_eq!(decoded.own_profile_since, 0);
	assert!(!decoded.profiles_fields_widened);

	let current = Connection { own_profile_since: 9, ..decoded };
	let bytes = to_vec(&current).expect("current connection must encode");
	let downgraded: ConnectionV0 = from_slice(&bytes).expect("old binary must decode");

	assert_eq!(downgraded.next_batch, 8);
	assert_eq!(downgraded.rooms[room_id].roomsince, 7);
}

#[test]
fn previous_connection_cbor_defaults_required_state() {
	let room_id = room_id!("!legacy:example.com");
	let room = RoomV1 { roomsince: 7, config_hash: 19 };
	let legacy = ConnectionV1 {
		globalsince: 5,
		next_batch: 8,
		lists: Default::default(),
		extensions: Default::default(),
		subscriptions: Default::default(),
		rooms: [(room_id.to_owned(), room)].into(),
	};

	let bytes = to_vec(&legacy).expect("previous connection must encode");
	let decoded: Connection = from_slice(&bytes).expect("previous connection must decode");

	assert_eq!(decoded.globalsince, 5);
	assert_eq!(decoded.next_batch, 8);
	assert_eq!(decoded.rooms[room_id].roomsince, 7);
	assert_eq!(decoded.rooms[room_id].config_hash, 19);
	assert!(decoded.rooms[room_id].required_state.is_empty());
	assert_eq!(decoded.rooms[room_id].shortstatehash, 0);
}

#[test]
fn connection_cbor_preserves_required_state_and_allows_downgrade() {
	let room_id = room_id!("!stored:example.com");
	let room = Room {
		roomsince: 7,
		config_hash: 19,
		required_state: (1..=32).collect(),
		shortstatehash: 23,
		..Default::default()
	};

	let conn = Connection {
		globalsince: 5,
		next_batch: 8,
		rooms: [(room_id.to_owned(), room)].into(),
		..Default::default()
	};

	let bytes = to_vec(&conn).expect("connection must encode");
	let decoded: Connection = from_slice(&bytes).expect("connection must decode");
	let downgraded: ConnectionV1 = from_slice(&bytes).expect("previous reader must decode");

	assert_eq!(decoded.globalsince, 5);
	assert_eq!(decoded.next_batch, 8);
	assert_eq!(decoded.rooms[room_id].roomsince, 7);
	assert_eq!(decoded.rooms[room_id].config_hash, 19);
	assert_eq!(decoded.rooms[room_id].required_state, conn.rooms[room_id].required_state);
	assert_eq!(decoded.rooms[room_id].shortstatehash, 23);
	assert_eq!(downgraded.globalsince, 5);
	assert_eq!(downgraded.next_batch, 8);
	assert_eq!(downgraded.rooms[room_id].roomsince, 7);
	assert_eq!(downgraded.rooms[room_id].config_hash, 19);
}

#[test]
fn room_cbor_defaults_snapshot_and_allows_downgrade() {
	let previous = RoomV2 {
		roomsince: 7,
		config_hash: 19,
		required_state: (1..=4).collect(),
	};

	let bytes = to_vec(&previous).expect("previous room must encode");

	assert_eq!(bytes, ROOM_V2);

	let decoded: Room = from_slice(ROOM_V2).expect("previous room must decode");

	assert_eq!(decoded.roomsince, 7);
	assert_eq!(decoded.config_hash, 19);
	assert_eq!(decoded.required_state, previous.required_state);
	assert_eq!(decoded.shortstatehash, 0);

	let current = Room { shortstatehash: 23, ..decoded };
	let bytes = to_vec(&current).expect("room must encode");
	let downgraded: RoomV2 = from_slice(&bytes).expect("previous reader must decode");

	assert_eq!(downgraded.roomsince, 7);
	assert_eq!(downgraded.config_hash, 19);
	assert_eq!(downgraded.required_state, previous.required_state);
}

#[test]
fn room_cbor_writes_unread_state_only_after_a_failed_read() {
	let room = Room {
		roomsince: 7,
		config_hash: 19,
		required_state: (1..=4).collect(),
		shortstatehash: 23,
		..Default::default()
	};

	let previous = RoomV3 {
		roomsince: 7,
		config_hash: 19,
		required_state: (1..=4).collect(),
		shortstatehash: 23,
	};

	let bytes = to_vec(&room).expect("room must encode");

	assert_eq!(bytes, to_vec(&previous).expect("previous room must encode"));

	let unread = Room { unread_state: [5, 6].into(), ..room };

	let bytes = to_vec(&unread).expect("room must encode");
	let decoded: Room = from_slice(&bytes).expect("room must decode");
	let downgraded: RoomV3 = from_slice(&bytes).expect("previous reader must decode");

	assert_eq!(decoded.unread_state, unread.unread_state);
	assert_eq!(decoded.shortstatehash, 23);
	assert_eq!(downgraded.shortstatehash, 23);
	assert_eq!(downgraded.required_state, previous.required_state);
}

#[test]
fn room_cbor_leaves_out_the_checkpoint() {
	let room_id = room_id!("!checkpointed:example.com");
	let mut conn = Connection { next_batch: 9, ..Default::default() };

	conn.update_rooms_epilogue(5, delivery(room_id, (23, &[4], 31)));

	let room = &conn.rooms[room_id];
	let bytes = to_vec(room).expect("room must encode");
	let decoded: Room = from_slice(&bytes).expect("room must decode");
	let without_checkpoint = Room { checkpoint: None, ..room.clone() };

	assert!(room.checkpoint.is_some());
	assert!(decoded.checkpoint.is_none());
	assert_eq!(bytes, to_vec(&without_checkpoint).expect("room must encode"));
	assert_eq!(decoded.roomsince, 9);
	assert_eq!(decoded.config_hash, 23);
}

#[test]
fn device_removal_needs_a_confirmed_absence() {
	assert!(matches!(confirmed_removed(None), Ok(false)));
	assert!(matches!(confirmed_removed(Some(Ok(true))), Ok(false)));
	assert!(matches!(confirmed_removed(Some(Ok(false))), Ok(true)));
	confirmed_removed(Some(Err!(Database("unreadable"))))
		.expect_err("a failed lookup must stay an error");
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

fn request_with_profiles(enabled: bool) -> Request {
	let mut request = Request::new();

	request.extensions.profiles.enabled = Some(enabled);

	request
}

fn request_with_fields(fields: &[ProfileFieldName]) -> Request {
	let mut request = request_with_profiles(true);

	request.extensions.profiles.fields = Some(fields.to_owned());

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

fn delivery<'a>(
	room_id: &'a RoomId,
	(config_hash, required_state, shortstatehash): (u64, &[u64], u64),
) -> Once<RoomUpdate<'a>> {
	let required_state = required_state.iter().copied().collect();
	let config = (config_hash, required_state, shortstatehash, UnreadState::new());

	once((room_id, Some(config)))
}

fn connection_with_room(room_id: &RoomId) -> Connection {
	connection_with(room_id, delivered_room())
}

fn connection_with(room_id: &RoomId, room: Room) -> Connection {
	Connection {
		next_batch: 9,
		rooms: [(room_id.to_owned(), room)].into(),
		..Default::default()
	}
}

fn delivered_room() -> Room {
	Room {
		roomsince: 5,
		config_hash: 19,
		required_state: once(4).collect(),
		shortstatehash: 29,
		..Default::default()
	}
}

#[track_caller]
fn assert_delivery(
	room: &Room,
	roomsince: u64,
	(config_hash, required_state, shortstatehash): (u64, &[u64], u64),
) {
	assert_eq!(room.roomsince, roomsince);
	assert_eq!(room.config_hash, config_hash);
	assert_eq!(room.required_state.as_slice(), required_state);
	assert_eq!(room.shortstatehash, shortstatehash);
}

fn uint(value: u64) -> UInt { UInt::new(value).expect("range value must fit UInt") }

fn list_id() -> ListId { LIST_ID.into() }
