use std::{collections::BTreeMap, mem::take};

use futures::{StreamExt, TryFutureExt, TryStreamExt, future::try_join4};
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
	utils::{IterStream, TryReadyExt, stream::BroadbandExt},
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
#[inline]
pub(super) fn contains(&self, room_id: &RoomId) -> bool { self.ranges.contains_key(room_id) }

#[implement(Results)]
pub(super) fn retain_complete<T>(
	&self,
	rooms: &mut BTreeMap<OwnedRoomId, T>,
	attempted: impl Fn(&RoomId) -> bool,
) {
	rooms.retain(|room_id, _| !attempted(room_id) || self.contains(room_id));
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

	let payload = handle_room(sync_info, conn, window_room, roomsince).map_err(Failure::from);
	let public_receipts = public_receipts(sync_info, conn, room_id, roomsince, ignored)
		.map_err(|error| Failure::new(Domain::PublicReceipt, error));
	let private_receipts = private_receipts(sync_info, conn, room_id, roomsince)
		.map_err(|error| Failure::new(Domain::PrivateRead, error));
	let account_data = room_account_data(sync_info, conn, room_id, roomsince)
		.map_err(|error| Failure::new(Domain::RoomAccountData, error));

	let (payload, public_receipts, private_receipts, account_data) =
		try_join4(payload, public_receipts, private_receipts, account_data).await?;

	assemble(Ok(payload), Ok(public_receipts), Ok(private_receipts), Ok(account_data))
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

	if update <= roomsince {
		return Ok(PrivateReadEvents::new());
	}

	if update > conn.next_batch {
		return Err(err!(Database("Private read advanced beyond the bounded sync range.")));
	}

	let receipts = services
		.read_receipt
		.private_read_get_fallible(room_id, sender_user, update)
		.await?;

	let confirmed = services
		.read_receipt
		.last_privateread_update_fallible(sender_user, room_id)
		.await?;

	if confirmed != update {
		return Err(err!(Database(
			"Private read changed while assembling a bounded sync range."
		)));
	}

	Ok(receipts)
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
	payload: Result<response::Room, Failure>,
	public_receipts: Result<PublicReceipts, Failure>,
	private_receipts: Result<PrivateReadEvents, Failure>,
	account_data: Result<Vec<Raw<AnyRoomAccountDataEvent>>, Failure>,
) -> Result<CompleteRange, Failure>
where
	PublicReceipts: Iterator<Item = Raw<AnySyncEphemeralRoomEvent>>,
{
	let payload = payload?;
	let public_receipts = public_receipts?;
	let private_receipts = private_receipts?;
	let account_data = account_data?;

	let mut receipts = public_receipts.chain(private_receipts).peekable();

	let receipts = receipts
		.peek()
		.is_some()
		.then(|| pack_receipts_fallible(receipts))
		.transpose()
		.map_err(|error| Failure::new(Domain::ReceiptSerialization, error))?;

	Ok(CompleteRange {
		payload: Some(payload),
		receipts,
		account_data,
	})
}

#[cfg(test)]
mod tests {
	use std::vec::IntoIter;

	use ruma::{api::client::sync::sync_events::v5::response::Room as ResponseRoom, room_id};
	use serde_json::{json, value::to_raw_value};

	use super::*;

	#[test]
	fn failures_withhold_all_range_output_until_retry_succeeds() {
		let room_id = room_id!("!range:example.com");

		for domain in [
			Domain::Timeline,
			Domain::Payload,
			Domain::PublicReceipt,
			Domain::PrivateRead,
			Domain::RoomAccountData,
		] {
			let mut failed = publish(room_id, assemble_with_failure(domain));
			let mut typing = [(room_id.to_owned(), ())].into();
			failed.retain_complete(&mut typing, |_| true);

			assert!(!failed.contains(room_id));
			assert!(typing.is_empty());
			assert!(failed.take_receipts(room_id).is_none());
			assert!(failed.take_account_data(room_id).is_none());
			assert!(failed.into_payloads().is_empty());

			let retried = publish(room_id, successful_range());
			let mut typing = [(room_id.to_owned(), ())].into();
			retried.retain_complete(&mut typing, |_| true);

			assert!(retried.contains(room_id));
			assert_eq!(typing.len(), 1);
			assert_eq!(retried.into_payloads().len(), 1);
		}
	}

	#[test]
	fn unattempted_extension_rooms_are_preserved() {
		let attempted = room_id!("!attempted:example.com");
		let unattempted = room_id!("!unattempted:example.com");
		let failed = publish(attempted, assemble_with_failure(Domain::Timeline));
		let mut typing = [(attempted.to_owned(), ()), (unattempted.to_owned(), ())].into();

		failed.retain_complete(&mut typing, |room_id| room_id == attempted);

		assert!(!typing.contains_key(attempted));
		assert!(typing.contains_key(unattempted));
	}

	#[test]
	fn malformed_receipt_withholds_the_complete_range() {
		let room_id = room_id!("!receipt:example.com");
		let malformed = Raw::from_json(
			to_raw_value(&json!({"content": 5})).expect("test JSON should serialize"),
		);

		let range = assemble(
			Ok(ResponseRoom::default()),
			Ok(vec![malformed].into_iter()),
			Ok(PrivateReadEvents::new()),
			Ok(Vec::new()),
		);
		let error = range.expect_err("malformed receipt must fail the complete range");

		assert_eq!(error.domain, Domain::ReceiptSerialization);
		assert!(!publish(room_id, Err(error)).contains(room_id));
	}

	fn publish(room_id: &RoomId, range: Result<CompleteRange, Failure>) -> Results {
		let ranges = range
			.ok()
			.map(|range| (room_id.to_owned(), range))
			.into_iter()
			.collect();

		Results { ranges }
	}

	fn assemble_with_failure(domain: Domain) -> Result<CompleteRange, Failure> {
		let failure = || Failure::new(domain, Error::Err("injected failure".into()));

		match domain {
			| Domain::Timeline | Domain::Payload => assemble(
				Err(failure()),
				Ok(Vec::new().into_iter()),
				Ok(PrivateReadEvents::new()),
				Ok(Vec::new()),
			),
			| Domain::PublicReceipt => assemble(
				Ok(ResponseRoom::default()),
				Err::<IntoIter<Raw<AnySyncEphemeralRoomEvent>>, _>(failure()),
				Ok(PrivateReadEvents::new()),
				Ok(Vec::new()),
			),
			| Domain::PrivateRead => assemble(
				Ok(ResponseRoom::default()),
				Ok(Vec::new().into_iter()),
				Err(failure()),
				Ok(Vec::new()),
			),
			| Domain::RoomAccountData => assemble(
				Ok(ResponseRoom::default()),
				Ok(Vec::new().into_iter()),
				Ok(PrivateReadEvents::new()),
				Err(failure()),
			),
			| Domain::ReceiptSerialization => unreachable!("injected through malformed input"),
		}
	}

	fn successful_range() -> Result<CompleteRange, Failure> {
		assemble(
			Ok(ResponseRoom::default()),
			Ok(Vec::new().into_iter()),
			Ok(PrivateReadEvents::new()),
			Ok(Vec::new()),
		)
	}
}
