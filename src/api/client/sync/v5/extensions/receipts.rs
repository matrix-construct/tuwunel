use ruma::api::client::sync::sync_events::v5::response::Receipts;

use super::{Connection, Window, selector};
use crate::client::sync::v5::range::Results;

pub(super) fn collect_ranges(
	conn: &Connection,
	window: &Window,
	ranges: &mut Results,
) -> Receipts {
	let implicit = conn
		.extensions
		.receipts
		.lists
		.as_deref()
		.map(<[_]>::iter);

	let explicit = conn
		.extensions
		.receipts
		.rooms
		.as_deref()
		.map(<[_]>::iter);

	let rooms = selector(conn, window, implicit, explicit)
		.filter_map(|room_id| {
			ranges
				.take_receipts(room_id)
				.map(|event| (room_id.to_owned(), event))
		})
		.collect();

	Receipts { rooms }
}
