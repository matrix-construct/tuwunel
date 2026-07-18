use ruma::{
	api::client::sync::sync_events::v5::{ListId, request},
	room_id, uint,
};
use tuwunel_service::sync::{Connection, Room};

use super::{ListIds, ResponseLists, WindowRoom, list_selections};

fn list_id() -> ListId { "all_rooms".into() }

fn list_ids(list_id: &ListId) -> ListIds {
	let mut lists = ListIds::new();
	lists.push(list_id.clone());
	lists
}

#[test]
fn gated_list_window_excludes_room_without_persistent_delta() {
	let list_id = list_id();
	let room_id = room_id!("!quiet:example.com").to_owned();
	let room = WindowRoom {
		room_id: room_id.clone(),
		membership: None,
		lists: list_ids(&list_id),
		ranked: 0,
		last_count: 10,
	};

	let mut conn = Connection::default();
	conn.lists.insert(list_id.clone(), request::List {
		ranges: [(uint!(0), uint!(0))].into_iter().collect(),
		..Default::default()
	});
	conn.rooms
		.insert(room_id.clone(), Room { roomsince: 20 });

	let mut response_lists = ResponseLists::new();
	response_lists.insert(list_id, Default::default());
	let rooms = [room];

	let gated = list_selections(&conn, rooms.iter(), &response_lists, true).collect::<Vec<_>>();
	assert!(gated.is_empty());

	let ungated = list_selections(&conn, rooms.iter(), &response_lists, false)
		.map(|(room_id, _)| room_id)
		.collect::<Vec<_>>();
	assert_eq!(ungated, [room_id]);
}

#[test]
fn gated_list_window_includes_room_with_persistent_delta() {
	let list_id = list_id();
	let room_id = room_id!("!active:example.com").to_owned();
	let room = WindowRoom {
		room_id: room_id.clone(),
		membership: None,
		lists: list_ids(&list_id),
		ranked: 0,
		last_count: 30,
	};

	let mut conn = Connection::default();
	conn.lists.insert(list_id.clone(), request::List {
		ranges: [(uint!(0), uint!(0))].into_iter().collect(),
		..Default::default()
	});
	conn.rooms
		.insert(room_id.clone(), Room { roomsince: 20 });

	let mut response_lists = ResponseLists::new();
	response_lists.insert(list_id, Default::default());
	let rooms = [room];

	let gated = list_selections(&conn, rooms.iter(), &response_lists, true)
		.map(|(room_id, _)| room_id)
		.collect::<Vec<_>>();
	assert_eq!(gated, [room_id]);
}
