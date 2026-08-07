use futures::{FutureExt, StreamExt};
use ruma::{
	OwnedRoomId, RoomId,
	api::client::sync::sync_events::v5::response::Receipts,
	events::{AnySyncEphemeralRoomEvent, receipt::SyncReceiptEvent},
	serde::Raw,
};
use tuwunel_core::{
	Result,
	utils::{BoolExt, IterStream, stream::BroadbandExt},
};
use tuwunel_service::{
	rooms::read_receipt::{PrivateReadEvents, pack_receipts},
	sync::Room,
};

use super::{Connection, SyncInfo, Window, selector};
use crate::client::sync::v5::range::Results;

#[tracing::instrument(name = "receipts", level = "trace", skip_all)]
pub(super) async fn collect(
	sync_info: SyncInfo<'_>,
	conn: &Connection,
	window: &Window,
) -> Result<Receipts> {
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
		.filter(|&room_id| !window.contains_key(room_id))
		.stream()
		.broad_filter_map(|room_id| collect_room(sync_info, conn, room_id))
		.collect()
		.await;

	Ok(Receipts { rooms })
}

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

#[tracing::instrument(level = "trace", skip_all, fields(room_id), ret)]
async fn collect_room(
	SyncInfo { services, sender_user, .. }: SyncInfo<'_>,
	conn: &Connection,
	room_id: &RoomId,
) -> Option<(OwnedRoomId, Raw<SyncReceiptEvent>)> {
	let &Room { roomsince, .. } = conn.rooms.get(room_id)?;
	let private_receipt = services
		.read_receipt
		.last_privateread_update(sender_user, room_id)
		.then(async |last_private_update| {
			if last_private_update <= roomsince || last_private_update > conn.next_batch {
				return PrivateReadEvents::new();
			}

			services
				.read_receipt
				.private_read_get(room_id, sender_user)
				.await
				.unwrap_or_default()
		})
		.map(IterStream::stream)
		.flatten_stream();

	let receipts: Vec<Raw<AnySyncEphemeralRoomEvent>> = services
		.read_receipt
		.readreceipts_since(room_id, roomsince, Some(conn.next_batch))
		.filter_map(async |(read_user, _ts, event)| {
			services
				.users
				.user_is_ignored(read_user, sender_user)
				.await
				.or_some(event)
		})
		.chain(private_receipt)
		.collect()
		.boxed()
		.await;

	(!receipts.is_empty()).then(|| (room_id.to_owned(), pack_receipts(receipts.into_iter())))
}
