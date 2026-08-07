use std::collections::BTreeMap;

use futures::{StreamExt, TryStreamExt, future::join};
use ruma::{
	OwnedRoomId,
	api::client::sync::sync_events::v5::response::AccountData,
	events::{AnyRawAccountDataEvent, AnyRoomAccountDataEvent},
	serde::Raw,
};
use tuwunel_core::{
	Result, extract_variant,
	utils::{IterStream, ReadyExt, TryReadyExt, stream::BroadbandExt},
};
use tuwunel_service::sync::Room;

use super::{Connection, SyncInfo, Window, selector};
use crate::client::{is_empty_account_data_event, sync::v5::range::Results};

#[tracing::instrument(name = "account_data", level = "trace", skip_all)]
pub(super) async fn collect(
	sync_info: SyncInfo<'_>,
	conn: &Connection,
	window: &Window,
) -> Result<AccountData> {
	let global = collect_global(sync_info, conn);
	let rooms = collect_unattempted(sync_info, conn, window);
	let (global, rooms) = join(global, rooms).await;
	let mut account_data = global?;

	account_data.rooms = rooms;

	Ok(account_data)
}

async fn collect_global(
	SyncInfo { services, sender_user, .. }: SyncInfo<'_>,
	conn: &Connection,
) -> Result<AccountData> {
	let globalsince = conn.globalsince;
	let global = services
		.account_data
		.changes_since_fallible(None, sender_user, globalsince, Some(conn.next_batch))
		.ready_try_filter_map(|event| Ok(extract_variant!(event, AnyRawAccountDataEvent::Global)))
		.ready_try_filter(move |event| globalsince != 0 || !is_empty_account_data_event(event))
		.try_collect()
		.await?;

	Ok(AccountData { global, rooms: Default::default() })
}

async fn collect_unattempted(
	SyncInfo { services, sender_user, .. }: SyncInfo<'_>,
	conn: &Connection,
	window: &Window,
) -> BTreeMap<OwnedRoomId, Vec<Raw<AnyRoomAccountDataEvent>>> {
	let implicit = conn
		.extensions
		.account_data
		.lists
		.as_deref()
		.map(<[_]>::iter);

	let explicit = conn
		.extensions
		.account_data
		.rooms
		.as_deref()
		.map(<[_]>::iter);

	selector(conn, window, implicit, explicit)
		.filter(|&room_id| !window.contains_key(room_id))
		.stream()
		.broad_filter_map(async |room_id| {
			let &Room { roomsince, .. } = conn.rooms.get(room_id)?;
			let changes: Vec<_> = services
				.account_data
				.changes_since(Some(room_id), sender_user, roomsince, Some(conn.next_batch))
				.ready_filter_map(|event| extract_variant!(event, AnyRawAccountDataEvent::Room))
				.ready_filter(move |event| roomsince != 0 || !is_empty_account_data_event(event))
				.collect()
				.await;

			(!changes.is_empty()).then(|| (room_id.to_owned(), changes))
		})
		.collect()
		.await
}

pub(super) fn collect_ranges(
	conn: &Connection,
	window: &Window,
	ranges: &mut Results,
) -> BTreeMap<OwnedRoomId, Vec<Raw<AnyRoomAccountDataEvent>>> {
	let implicit = conn
		.extensions
		.account_data
		.lists
		.as_deref()
		.map(<[_]>::iter);

	let explicit = conn
		.extensions
		.account_data
		.rooms
		.as_deref()
		.map(<[_]>::iter);

	selector(conn, window, implicit, explicit)
		.filter_map(|room_id| {
			ranges
				.take_account_data(room_id)
				.map(|events| (room_id.to_owned(), events))
		})
		.collect()
}
