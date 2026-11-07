use std::slice::Iter;

use ruma::{events::room::member::MembershipState, room_id};
use tuwunel_service::sync::Room;

use super::*;
use crate::sync::v5::ListIds;

#[test]
fn explicit_room_outside_the_window_is_rejected() {
	let selected = room_id!("!selected:example.com");
	let foreign = room_id!("!foreign:example.com");
	let window = window(selected, 0);
	let conn = Connection::default();
	let lists = [ListId::from("unmatched")];
	let rooms = [ExtensionRoomConfig::Room(foreign.to_owned())];

	assert_eq!(listed(&conn, &window, &lists, &rooms).len(), 0);
}

#[test]
fn all_subscribed_room_outside_the_window_is_rejected() {
	let selected = room_id!("!selected:example.com");
	let foreign = room_id!("!foreign:example.com");
	let window = window(selected, 0);
	let conn = subscribed(foreign);
	let lists = [ListId::from("unmatched")];
	let rooms = [ExtensionRoomConfig::AllSubscribed];

	assert_eq!(listed(&conn, &window, &lists, &rooms).len(), 0);
}

#[test]
fn stale_payload_room_in_the_window_remains_extension_eligible() {
	let room_id = room_id!("!stale:example.com");
	let window = window(room_id, 1);
	let delivered = Room { roomsince: 9, ..Default::default() };
	let conn = Connection {
		rooms: [(room_id.to_owned(), delivered)].into(),
		..Default::default()
	};

	assert_eq!(eligible(&conn, &window), [room_id]);
}

#[test]
fn departed_room_remains_extension_eligible() {
	let room_id = room_id!("!departed:example.com");
	let membership = Some(MembershipState::Leave);
	let window = window_of(WindowRoom { membership, ..room(room_id, 1) });

	assert_eq!(eligible(&Connection::default(), &window), [room_id]);
}

#[test]
fn forgotten_room_is_not_extension_eligible() {
	let room_id = room_id!("!forgotten:example.com");
	let window = window_of(WindowRoom { forgotten: true, ..room(room_id, 1) });

	assert_eq!(eligible(&Connection::default(), &window).len(), 0);
}

#[test]
fn explicit_room_already_selected_by_list_is_not_duplicated() {
	let room_id = room_id!("!explicit-overlap:example.com");
	let list = ListId::from("main");
	let window = window_of(WindowRoom {
		lists: [list.clone()].into(),
		..room(room_id, 0)
	});

	let conn = Connection::default();
	let lists = [list];
	let rooms = [ExtensionRoomConfig::Room(room_id.to_owned())];

	assert_eq!(listed(&conn, &window, &lists, &rooms), [room_id]);
}

#[test]
fn subscribed_room_already_selected_by_list_is_not_duplicated() {
	let room_id = room_id!("!subscribed-overlap:example.com");
	let list = ListId::from("main");
	let window = window_of(WindowRoom {
		lists: [list.clone()].into(),
		..room(room_id, 0)
	});

	let conn = subscribed(room_id);
	let lists = [list];
	let rooms = [ExtensionRoomConfig::AllSubscribed];

	assert_eq!(listed(&conn, &window, &lists, &rooms), [room_id]);
}

fn window(room_id: &RoomId, payload_count: u64) -> Window {
	window_of(room(room_id, payload_count))
}

fn listed<'a>(
	conn: &'a Connection,
	window: &'a Window,
	lists: &'a [ListId],
	rooms: &'a [ExtensionRoomConfig],
) -> Vec<&'a RoomId> {
	selector(conn, window, Some(lists.iter()), Some(rooms.iter())).collect()
}

fn subscribed(room_id: &RoomId) -> Connection {
	Connection {
		subscriptions: [(room_id.to_owned(), Default::default())].into(),
		..Default::default()
	}
}

fn eligible<'a>(conn: &'a Connection, window: &'a Window) -> Vec<&'a RoomId> {
	let lists: Option<Iter<'_, ListId>> = None;
	let rooms: Option<Iter<'_, ExtensionRoomConfig>> = None;

	selector(conn, window, lists, rooms).collect()
}

fn window_of(room: WindowRoom) -> Window { [(room.room_id.clone(), room)].into() }

fn room(room_id: &RoomId, payload_count: u64) -> WindowRoom {
	WindowRoom {
		room_id: room_id.to_owned(),
		membership: None,
		forgotten: false,
		lists: ListIds::new(),
		event_count: 0,
		payload_count,
	}
}
