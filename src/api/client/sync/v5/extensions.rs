mod account_data;
mod e2ee;
mod receipts;
mod to_device;
mod typing;

use std::{collections::BTreeMap, fmt::Debug};

use futures::{FutureExt, future::join4};
use ruma::{
	OwnedRoomId, RoomId,
	api::client::sync::sync_events::v5::{
		ListId,
		request::ExtensionRoomConfig,
		response::{Extensions, Room as ResponseRoom},
	},
};
use tuwunel_core::{Result, apply, at, extract_variant, utils::BoolExt};
use tuwunel_service::sync::Connection;

use self::{
	account_data::{
		collect as collect_account_data, collect_ranges as collect_account_data_ranges,
	},
	receipts::collect_ranges as collect_receipt_ranges,
};
use super::{SyncInfo, Window, WindowRoom, range::Results, share_encrypted_room};

pub(super) struct Collected {
	response: Extensions,
	typing: typing::Collected,
}

impl Collected {
	pub(super) fn into_response(
		mut self,
		payloads: &BTreeMap<OwnedRoomId, ResponseRoom>,
	) -> Extensions {
		self.response.typing = self.typing.into_response(payloads);
		self.response
	}
}

#[tracing::instrument(
	name = "extensions",
	level = "debug",
	skip_all,
	fields(
		next_batch = conn.next_batch,
		window = window.len(),
		rooms = conn.rooms.len(),
		subs = conn.subscriptions.len(),
	)
)]
pub(super) async fn handle(
	sync_info: SyncInfo<'_>,
	conn: &Connection,
	window: &Window,
) -> Result<Collected> {
	let account_data = conn
		.extensions
		.account_data
		.enabled
		.unwrap_or(false)
		.then_async(|| collect_account_data(sync_info, conn));

	let typing = conn
		.extensions
		.typing
		.enabled
		.unwrap_or(false)
		.then_async(|| typing::collect(sync_info, conn, window));

	let to_device = conn
		.extensions
		.to_device
		.enabled
		.unwrap_or(false)
		.then_async(|| to_device::collect(sync_info, conn));

	let e2ee = conn
		.extensions
		.e2ee
		.enabled
		.unwrap_or(false)
		.then_async(|| e2ee::collect(sync_info, conn));

	let (account_data, typing, to_device, e2ee) = join4(account_data, typing, to_device, e2ee)
		.map(apply!(4, |t: Option<_>| t.unwrap_or(Ok(Default::default()))))
		.await;

	// Receipt and room account-data payloads only exist as bounded room-range
	// outputs, applied by `apply_ranges` after the ranges resolve.
	let response = Extensions {
		account_data: account_data?,
		receipts: Default::default(),
		typing: Default::default(),
		to_device: to_device?,
		e2ee: e2ee?,
	};

	Ok(Collected { response, typing: typing? })
}

pub(super) fn apply_ranges(
	conn: &Connection,
	window: &Window,
	ranges: &mut Results,
	extensions: &mut Collected,
) {
	if conn.extensions.receipts.enabled.unwrap_or(false) {
		extensions.response.receipts = collect_receipt_ranges(conn, window, ranges);
	}

	if conn
		.extensions
		.account_data
		.enabled
		.unwrap_or(false)
	{
		extensions.response.account_data.rooms =
			collect_account_data_ranges(conn, window, ranges);
	}
}

#[tracing::instrument(
	name = "selector",
	level = "trace",
	skip_all,
	fields(?implicit, ?explicit),
)]
fn selector<'a, ListIter, SubsIter>(
	conn: &'a Connection,
	window: &'a Window,
	implicit: Option<ListIter>,
	explicit: Option<SubsIter>,
) -> impl Iterator<Item = &'a RoomId> + Send + Sync + 'a
where
	ListIter: Iterator<Item = &'a ListId> + Clone + Debug + Send + Sync + 'a,
	SubsIter: Iterator<Item = &'a ExtensionRoomConfig> + Clone + Debug + Send + Sync + 'a,
{
	let has_all_subscribed = explicit
		.clone()
		.into_iter()
		.flatten()
		.any(|erc| matches!(erc, ExtensionRoomConfig::AllSubscribed));
	let implicit_subscribed = implicit.clone();
	let implicit_explicit = implicit.clone();

	let all_subscribed = has_all_subscribed
		.then(|| {
			window
				.keys()
				.filter(|room_id| conn.subscriptions.contains_key(*room_id))
				.filter(move |room_id| {
					window
						.get(*room_id)
						.is_some_and(|room| !implicit_match(room, implicit_subscribed.as_ref()))
				})
		})
		.into_iter()
		.flatten()
		.map(AsRef::as_ref);

	let rooms_explicit = has_all_subscribed
		.is_false()
		.then(move || {
			explicit
				.into_iter()
				.flatten()
				.filter_map(|erc| extract_variant!(erc, ExtensionRoomConfig::Room))
				.filter(move |room_id| {
					window
						.get::<RoomId>(room_id.as_ref())
						.is_some_and(|room| !implicit_match(room, implicit_explicit.as_ref()))
				})
				.map(AsRef::as_ref)
		})
		.into_iter()
		.flatten();

	let rooms_selected = window
		.iter()
		.filter(move |(_, room)| implicit_match(room, implicit.as_ref()))
		.map(at!(0))
		.map(AsRef::as_ref);

	all_subscribed
		.chain(rooms_explicit)
		.chain(rooms_selected)
}

fn implicit_match<'a, ListIter>(room: &WindowRoom, implicit: Option<&ListIter>) -> bool
where
	ListIter: Iterator<Item = &'a ListId> + Clone,
{
	implicit.is_none_or(|lists| {
		lists
			.clone()
			.any(|list| room.lists.contains(list))
	})
}

#[cfg(test)]
mod tests {
	use std::slice::Iter;

	use ruma::room_id;

	use super::*;
	use crate::client::sync::v5::ListIds;

	#[test]
	fn explicit_room_outside_the_window_is_rejected() {
		let selected = room_id!("!selected:example.com");
		let foreign = room_id!("!foreign:example.com");
		let window = window(selected, 0);
		let conn = Connection::default();
		let list = ListId::from("unmatched");
		let lists = [list];
		let rooms = [ExtensionRoomConfig::Room(foreign.to_owned())];

		assert!(
			selector(&conn, &window, Some(lists.iter()), Some(rooms.iter()))
				.next()
				.is_none()
		);
	}

	#[test]
	fn all_subscribed_room_outside_the_window_is_rejected() {
		let selected = room_id!("!selected:example.com");
		let foreign = room_id!("!foreign:example.com");
		let window = window(selected, 0);
		let mut conn = Connection::default();
		conn.subscriptions
			.insert(foreign.to_owned(), Default::default());
		let list = ListId::from("unmatched");
		let lists = [list];
		let rooms = [ExtensionRoomConfig::AllSubscribed];

		assert!(
			selector(&conn, &window, Some(lists.iter()), Some(rooms.iter()))
				.next()
				.is_none()
		);
	}

	#[test]
	fn stale_payload_room_in_the_window_remains_extension_eligible() {
		let room_id = room_id!("!stale:example.com");
		let window = window(room_id, 1);
		let mut conn = Connection::default();
		conn.rooms
			.entry(room_id.to_owned())
			.or_default()
			.roomsince = 9;

		let lists: Option<Iter<'_, ListId>> = None;
		let rooms: Option<Iter<'_, ExtensionRoomConfig>> = None;

		let selected = selector(&conn, &window, lists, rooms).collect::<Vec<_>>();

		assert_eq!(selected, [room_id]);
	}

	#[test]
	fn explicit_room_already_selected_by_list_is_not_duplicated() {
		let room_id = room_id!("!explicit-overlap:example.com");
		let list = ListId::from("main");
		let mut window = window(room_id, 0);

		window
			.get_mut(room_id)
			.expect("test room should be present")
			.lists
			.push(list.clone());

		let conn = Connection::default();
		let lists = [list];
		let rooms = [ExtensionRoomConfig::Room(room_id.to_owned())];
		let selected =
			selector(&conn, &window, Some(lists.iter()), Some(rooms.iter())).collect::<Vec<_>>();

		assert_eq!(selected, [room_id]);
	}

	#[test]
	fn subscribed_room_already_selected_by_list_is_not_duplicated() {
		let room_id = room_id!("!subscribed-overlap:example.com");
		let list = ListId::from("main");
		let mut window = window(room_id, 0);

		window
			.get_mut(room_id)
			.expect("test room should be present")
			.lists
			.push(list.clone());

		let mut conn = Connection::default();
		conn.subscriptions
			.insert(room_id.to_owned(), Default::default());

		let lists = [list];
		let rooms = [ExtensionRoomConfig::AllSubscribed];
		let selected =
			selector(&conn, &window, Some(lists.iter()), Some(rooms.iter())).collect::<Vec<_>>();

		assert_eq!(selected, [room_id]);
	}

	fn window(room_id: &RoomId, payload_count: u64) -> Window {
		let room = WindowRoom {
			room_id: room_id.to_owned(),
			membership: None,
			lists: ListIds::new(),
			event_count: 0,
			payload_count,
		};

		[(room_id.to_owned(), room)].into()
	}
}
