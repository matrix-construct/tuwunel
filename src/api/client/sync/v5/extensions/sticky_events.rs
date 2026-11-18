use ruma::{
	RoomId,
	api::client::sync::sync_events::v5::{
		ListId, ListIds,
		request::StickyEvents as Config,
		response::{StickyEvents, StickyEventsRoom},
	},
};
use tuwunel_core::{Err, Result};

use super::{Connection, Window, selector};
use crate::sync::v5::range::Results;

/// Refuses a sticky events limit of zero, which MSC4480 puts below its least.
///
/// A positive limit is accepted and not applied: every unexpired sticky event
/// a room owes is delivered at once.
pub(in crate::sync::v5) fn check(request: &Config) -> Result {
	if request
		.limit
		.is_some_and(|limit| u64::from(limit).eq(&0))
	{
		return Err!(Request(BadJson("Sticky events limit must be positive.")));
	}

	Ok(())
}

pub(super) fn collect_ranges(
	conn: &Connection,
	window: &Window,
	ranges: &mut Results,
) -> StickyEvents {
	let config = &conn.extensions.sticky_events;

	// The extension names its lists as strings where its siblings take list ids.
	let lists: Option<ListIds> = config.lists.as_deref().map(|lists| {
		lists
			.iter()
			.map(String::as_str)
			.map(ListId::from)
			.collect()
	});

	let implicit = lists.as_deref().map(<[_]>::iter);
	let explicit = config.rooms.as_deref().map(<[_]>::iter);

	take_rooms(ranges, selector(conn, window, implicit, explicit))
}

fn take_rooms<'a, Rooms>(ranges: &mut Results, rooms: Rooms) -> StickyEvents
where
	Rooms: Iterator<Item = &'a RoomId>,
{
	let rooms = rooms
		.filter_map(|room_id| {
			ranges
				.take_sticky(room_id)
				.map(|events| (room_id.to_owned(), StickyEventsRoom { events }))
		})
		.collect();

	StickyEvents { rooms }
}
