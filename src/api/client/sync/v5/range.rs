use std::{collections::BTreeMap, mem::take};

use futures::{FutureExt, StreamExt, TryFutureExt, TryStreamExt, future::try_join4};
use ruma::{
	OwnedRoomId, OwnedUserId, RoomId,
	api::client::sync::sync_events::v5::response,
	events::{
		AnyRawAccountDataEvent, AnyRoomAccountDataEvent, AnySyncEphemeralRoomEvent,
		GlobalAccountDataEventType, ignored_user_list::IgnoredUserListEvent,
		receipt::SyncReceiptEvent,
	},
	serde::Raw,
};
use tokio::sync::OnceCell;
use tuwunel_core::{
	Error, Result, at, err, error, extract_variant, implement,
	utils::{BoolExt, IterStream, TryReadyExt, stream::BroadbandExt},
};
use tuwunel_service::{
	rooms::read_receipt::{PrivateReadEvents, pack_receipts_fallible},
	sync::Connection,
};

use super::{
	SyncInfo, Window, WindowRoom,
	rooms::{
		Failure as RoomFailure,
		Failure::{Payload as PayloadFailure, Timeline as TimelineFailure},
		handle_room,
	},
};
use crate::client::is_empty_account_data_event;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Domain {
	Timeline,
	Payload,
	PublicReceipt,
	PrivateRead,
	RoomAccountData,
	ReceiptSerialization,
}

#[derive(Debug)]
struct Failure {
	domain: Domain,
	error: Error,
}

impl Failure {
	fn new(domain: Domain, error: Error) -> Self { Self { domain, error } }
}

impl From<RoomFailure> for Failure {
	fn from(failure: RoomFailure) -> Self {
		match failure {
			| TimelineFailure(error) => Self::new(Domain::Timeline, error),
			| PayloadFailure(error) => Self::new(Domain::Payload, error),
		}
	}
}

#[derive(Debug)]
struct CompleteRange {
	payload: Option<response::Room>,
	receipts: Option<Raw<SyncReceiptEvent>>,
	account_data: Vec<Raw<AnyRoomAccountDataEvent>>,
}

#[derive(Default)]
pub(super) struct Results {
	ranges: BTreeMap<OwnedRoomId, CompleteRange>,
}

#[implement(Results)]
pub(super) fn keys(&self) -> impl Iterator<Item = &RoomId> {
	self.ranges.keys().map(AsRef::as_ref)
}

#[implement(Results)]
pub(super) fn into_payloads(self) -> BTreeMap<OwnedRoomId, response::Room> {
	self.ranges
		.into_iter()
		.filter_map(|(room_id, range)| range.payload.map(|payload| (room_id, payload)))
		.collect()
}

#[implement(Results)]
pub(super) fn take_receipts(&mut self, room_id: &RoomId) -> Option<Raw<SyncReceiptEvent>> {
	self.ranges
		.get_mut(room_id)
		.and_then(|range| range.receipts.take())
}

#[implement(Results)]
pub(super) fn take_account_data(
	&mut self,
	room_id: &RoomId,
) -> Option<Vec<Raw<AnyRoomAccountDataEvent>>> {
	self.ranges
		.get_mut(room_id)
		.map(|range| take(&mut range.account_data))
		.filter(|events| !events.is_empty())
}

#[tracing::instrument(
	name = "ranges",
	level = "debug",
	skip_all,
	fields(
		next_batch = conn.next_batch,
		window = window.len(),
	),
)]
pub(super) async fn collect(
	sync_info: SyncInfo<'_>,
	conn: &Connection,
	window: &Window,
) -> Results {
	let ignored = OnceCell::new();
	let ranges = window
		.iter()
		.stream()
		.broad_filter_map(async |(room_id, window_room)| {
			let roomsince = conn
				.rooms
				.get(room_id)
				.map(|room| room.roomsince)
				.unwrap_or_default();

			match collect_room(sync_info, conn, window_room, roomsince, &ignored).await {
				| Ok(range) => Some((room_id.clone(), range)),
				| Err(Failure { domain, error }) => {
					error!(
						%room_id,
						?domain,
						roomsince,
						next_batch = conn.next_batch,
						%error,
						"sliding sync range failed"
					);
					None
				},
			}
		})
		.collect()
		.await;

	Results { ranges }
}

async fn collect_room(
	sync_info: SyncInfo<'_>,
	conn: &Connection,
	window_room: &WindowRoom,
	roomsince: u64,
	ignored: &OnceCell<Option<IgnoredUserListEvent>>,
) -> Result<CompleteRange, Failure> {
	let room_id = &window_room.room_id;

	let payload = window_room
		.payload_is_fresh(roomsince)
		.then_async(|| handle_room(sync_info, conn, window_room, roomsince))
		.map(Option::transpose)
		.map_err(Failure::from);

	let public_receipts = public_receipts(sync_info, conn, room_id, roomsince, ignored)
		.map_err(|error| Failure::new(Domain::PublicReceipt, error));

	let private_receipts = private_receipts(sync_info, conn, room_id, roomsince)
		.map_err(|error| Failure::new(Domain::PrivateRead, error));

	let account_data = room_account_data(sync_info, conn, room_id, roomsince)
		.map_err(|error| Failure::new(Domain::RoomAccountData, error));

	let (payload, public_receipts, private_receipts, account_data) =
		try_join4(payload, public_receipts, private_receipts, account_data).await?;

	assemble(payload, public_receipts, private_receipts, account_data)
}

async fn public_receipts(
	SyncInfo { services, sender_user, .. }: SyncInfo<'_>,
	conn: &Connection,
	room_id: &RoomId,
	roomsince: u64,
	ignored: &OnceCell<Option<IgnoredUserListEvent>>,
) -> Result<impl Iterator<Item = Raw<AnySyncEphemeralRoomEvent>>> {
	let mut receipts: Vec<(OwnedUserId, Raw<AnySyncEphemeralRoomEvent>)> = services
		.read_receipt
		.readreceipts_since_fallible(room_id, roomsince, Some(conn.next_batch))
		.map_ok(|(user_id, _ts, event)| (user_id.to_owned(), event))
		.try_collect()
		.await?;

	if !receipts.is_empty() {
		let ignored = ignored
			.get_or_try_init(async || {
				services
					.account_data
					.get_global(sender_user, GlobalAccountDataEventType::IgnoredUserList)
					.await
					.map(Some)
					.or_else(|error| error.is_not_found().then_some(None).ok_or(error))
			})
			.await?;

		if let Some(ignored) = ignored {
			receipts.retain(|(user_id, _)| {
				!ignored
					.content
					.ignored_users
					.contains_key(user_id)
			});
		}
	}

	Ok(receipts.into_iter().map(at!(1)))
}

async fn private_receipts(
	SyncInfo { services, sender_user, .. }: SyncInfo<'_>,
	conn: &Connection,
	room_id: &RoomId,
	roomsince: u64,
) -> Result<PrivateReadEvents> {
	let update = services
		.read_receipt
		.last_privateread_update_fallible(sender_user, room_id)
		.await?;

	match update {
		| _ if update <= roomsince => Ok(PrivateReadEvents::new()),
		| _ if update > conn.next_batch =>
			Err(err!(Database("Private read advanced beyond the bounded sync range."))),
		| _ =>
			services
				.read_receipt
				.private_read_get_fallible(room_id, sender_user, update)
				.await,
	}
}

async fn room_account_data(
	SyncInfo { services, sender_user, .. }: SyncInfo<'_>,
	conn: &Connection,
	room_id: &RoomId,
	roomsince: u64,
) -> Result<Vec<Raw<AnyRoomAccountDataEvent>>> {
	services
		.account_data
		.changes_since_fallible(Some(room_id), sender_user, roomsince, Some(conn.next_batch))
		.ready_try_filter_map(|event| Ok(extract_variant!(event, AnyRawAccountDataEvent::Room)))
		.ready_try_filter(move |event| roomsince != 0 || !is_empty_account_data_event(event))
		.try_collect()
		.await
}

fn assemble<PublicReceipts>(
	payload: Option<response::Room>,
	public_receipts: PublicReceipts,
	private_receipts: PrivateReadEvents,
	account_data: Vec<Raw<AnyRoomAccountDataEvent>>,
) -> Result<CompleteRange, Failure>
where
	PublicReceipts: Iterator<Item = Raw<AnySyncEphemeralRoomEvent>>,
{
	let mut receipts = public_receipts.chain(private_receipts).peekable();
	let receipts = receipts
		.peek()
		.is_some()
		.then(|| pack_receipts_fallible(receipts))
		.transpose()
		.map_err(|error| Failure::new(Domain::ReceiptSerialization, error))?;

	Ok(CompleteRange { payload, receipts, account_data })
}

#[cfg(test)]
mod tests {
	use ruma::{api::client::sync::sync_events::v5::response::Room as ResponseRoom, room_id};
	use serde_json::{json, value::to_raw_value};

	use super::*;

	#[test]
	fn malformed_receipt_withholds_the_complete_range() {
		let room_id = room_id!("!receipt:example.com");
		let malformed = Raw::from_json(
			to_raw_value(&json!({"content": 5})).expect("test JSON should serialize"),
		);

		let range = assemble(
			Some(ResponseRoom::default()),
			vec![malformed].into_iter(),
			PrivateReadEvents::new(),
			Vec::new(),
		);

		let error = range.expect_err("malformed receipt must fail the complete range");

		assert_eq!(error.domain, Domain::ReceiptSerialization);
		assert!(
			!publish(room_id, Err(error))
				.ranges
				.contains_key(room_id)
		);
	}

	#[test]
	fn extension_only_range_commits_without_a_room_payload() {
		let room_id = room_id!("!extension-only:example.com");
		let range = assemble(None, Vec::new().into_iter(), PrivateReadEvents::new(), Vec::new());

		let range = publish(room_id, range);

		assert_eq!(range.keys().collect::<Vec<_>>(), [room_id]);
		assert!(range.into_payloads().is_empty());
	}

	#[test]
	fn extension_outputs_are_taken_once_without_removing_the_complete_range() {
		let room_id = room_id!("!extension-output:example.com");
		let receipt = Raw::from_json(
			to_raw_value(&json!({"content": {}})).expect("test receipt should serialize"),
		);

		let account_data = Raw::from_json(
			to_raw_value(&json!({"type": "m.tag", "content": {"tags": {}}}))
				.expect("test account data should serialize"),
		);

		let range = CompleteRange {
			payload: None,
			receipts: Some(receipt),
			account_data: vec![account_data],
		};

		let ranges = [(room_id.to_owned(), range)].into();
		let mut results = Results { ranges };

		assert!(results.take_receipts(room_id).is_some());
		assert!(results.take_receipts(room_id).is_none());

		let count = results
			.take_account_data(room_id)
			.map(|events| events.len());

		assert_eq!(Some(1), count);
		assert!(results.take_account_data(room_id).is_none());
		assert!(results.ranges.contains_key(room_id));
	}

	fn publish(room_id: &RoomId, range: Result<CompleteRange, Failure>) -> Results {
		let ranges = range
			.ok()
			.map(|range| (room_id.to_owned(), range))
			.into_iter()
			.collect();

		Results { ranges }
	}
}
