use std::{
	collections::BTreeMap,
	sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use futures::{Stream, StreamExt, pin_mut};
use ruma::{
	OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, ServerName, UserId,
	api::federation::transactions::edu::{Edu, ReceiptContent, ReceiptData, ReceiptMap},
	events::{
		AnySyncEphemeralRoomEvent,
		receipt::{Receipt, ReceiptEventContent, ReceiptType},
	},
	serde::Raw,
};
use tuwunel_core::{
	error, implement,
	smallvec::SmallVec,
	utils::{BoolExt, ReadyExt, stream::BroadbandExt},
};

use super::{Selected, edu_buf};
use crate::sending::{EduBuf, Service};

#[cfg(test)]
mod tests;

/// The `receipts` field of one `Edu::Receipt`, keyed by room.
///
/// Each room carries at most one `ReceiptData` per user.
type Receipts = BTreeMap<OwnedRoomId, ReceiptMap>;

/// Per-rank slice of receipt EDU output.
///
/// Each entry becomes one `Edu::Receipt` buffer; rank 0 carries each user's
/// earliest receipt in the window, rank 1 the next, and so on. Most windows
/// produce a single rank.
type RankedReceipts = SmallVec<[ReceiptMap; 1]>;

/// The ranks of every room gathered for one federation EDU window.
///
/// Most windows produce a single rank, so inline-1 avoids a heap touch.
type RankReceipts = SmallVec<[Receipts; 1]>;

const USER_LIMIT: usize = 256;

/// Select read-receipt EDUs across every room shared with the server.
///
/// MSC3771 lets a user emit multiple receipts in the same EDU window, one
/// per thread context. The federation EDU shape allows only one
/// `ReceiptData` per `(room, user)` slot, so a user with N parallel
/// thread receipts ships across N parallel `Edu::Receipt` buffers within
/// the same transaction. Each buffer is shape-compliant; receivers
/// process them as independent receipt EDUs and our storage keeps each
/// thread distinct.
#[implement(Service)]
#[tracing::instrument(
	name = "receipts",
	level = "trace",
	skip(self, server_name, max_edu_count, events_len)
)]
pub(super) async fn select_edus_receipts(
	&self,
	server_name: &ServerName,
	since: (u64, u64),
	max_edu_count: &AtomicU64,
	events_len: &AtomicUsize,
) -> Selected {
	let num = AtomicUsize::new(0);
	let by_rank = self
		.services
		.state_cache
		.server_rooms(server_name)
		.map(ToOwned::to_owned)
		.broad_filter_map(async |room_id| {
			let ranked = self
				.select_edus_receipts_room(&room_id, since, max_edu_count, &num)
				.await;

			ranked
				.is_empty()
				.is_false()
				.then_some((room_id, ranked))
		})
		.ready_fold(RankReceipts::new(), |mut by_rank, (room_id, ranked)| {
			for (rank, map) in ranked.into_iter().enumerate() {
				if rank >= by_rank.len() {
					by_rank.push(Receipts::new());
				}

				by_rank[rank].insert(room_id.clone(), map);
			}

			by_rank
		})
		.await;

	// Ranks reserve from the shared budget in order; those past the cap
	// overflow to the queue instead of truncating the tail.
	by_rank
		.into_iter()
		.map(serialize_edu)
		.fold(Selected::default(), |mut selected, edu| {
			selected.push(edu, events_len);
			selected
		})
}

/// Look for read receipts in this room.
///
/// The receipt-limit budget bounds distinct users only; subsequent thread
/// receipts for an already-counted user do not consume additional budget.
#[implement(Service)]
#[tracing::instrument(
	name = "receipts",
	level = "trace",
	skip(self, since, max_edu_count)
)]
async fn select_edus_receipts_room(
	&self,
	room_id: &RoomId,
	since: (u64, u64),
	max_edu_count: &AtomicU64,
	num: &AtomicUsize,
) -> RankedReceipts {
	let receipts = self
		.services
		.read_receipt
		.readreceipts_since(room_id, since.0, Some(since.1))
		.ready_filter_map(|(user_id, count, raw)| {
			debug_assert!(count <= since.1, "exceeds upper-bound");
			max_edu_count.fetch_max(count, Ordering::Relaxed);

			if !self.services.globals.user_is_local(user_id) {
				return None;
			}

			parse_receipt(user_id, count, &raw).map(|receipt| (user_id.to_owned(), receipt))
		});

	rank_receipts(receipts, num).await
}

/// Pivot a room's count-ordered receipts into rank-major order.
///
/// A user's k-th receipt in the window lands in rank k, so rank 0 is the first
/// receipt of every user and the budget counts a user once, on that rank. The
/// ranks holding a user are therefore a prefix, which is what lets the next
/// free rank be found by bisection.
async fn rank_receipts(
	receipts: impl Stream<Item = (OwnedUserId, ReceiptData)>,
	num: &AtomicUsize,
) -> RankedReceipts {
	pin_mut!(receipts);
	let mut ranked = RankedReceipts::new();

	while let Some((user_id, receipt)) = receipts.next().await {
		let rank = ranked.partition_point(|map| map.read.contains_key(&user_id));

		if rank == ranked.len() {
			ranked.push(ReceiptMap { read: BTreeMap::new() });
		}

		let prior = ranked[rank].read.insert(user_id, receipt);
		debug_assert!(prior.is_none(), "rank already holds this user");

		// The crossing user still ships: its count already advanced the watermark.
		if rank == 0 && num.fetch_add(1, Ordering::Relaxed) >= USER_LIMIT {
			break;
		}
	}

	ranked
}

fn serialize_edu(receipts: Receipts) -> EduBuf {
	edu_buf(&Edu::Receipt(ReceiptContent { receipts }))
}

fn parse_receipt(
	user_id: &UserId,
	count: u64,
	raw: &Raw<AnySyncEphemeralRoomEvent>,
) -> Option<ReceiptData> {
	let Ok(event) = raw.deserialize() else {
		error!(?user_id, ?count, ?raw, "Invalid edu event in read_receipts.");
		return None;
	};

	let AnySyncEphemeralRoomEvent::Receipt(receipt) = event else {
		error!(?user_id, ?count, ?event, "Invalid event type in read_receipts");
		return None;
	};

	let Some((event_id, data)) = own_read_receipt(receipt.content, user_id) else {
		error!(?user_id, ?count, "Read receipt event lacks the user's own receipt.");
		return None;
	};

	Some(ReceiptData { data, event_ids: vec![event_id] })
}

/// The event and receipt a user's own read receipt names.
///
/// Our stored receipts carry one event with one `Read` entry for the user;
/// anything else is malformed.
fn own_read_receipt(
	content: ReceiptEventContent,
	user_id: &UserId,
) -> Option<(OwnedEventId, Receipt)> {
	let (event_id, receipts) = content.0.into_iter().next()?;
	let data = receipts
		.into_iter()
		.find_map(|(kind, users)| (kind == ReceiptType::Read).then_some(users))?
		.into_iter()
		.find_map(|(id, data)| (id == user_id).then_some(data))?;

	Some((event_id, data))
}
