use std::{collections::BTreeMap, sync::Arc};

use futures::{
	Stream, StreamExt, TryStreamExt,
	future::{join, try_join},
};
use ruma::{
	CanonicalJsonObject, EventId, OwnedEventId, RoomId, UserId,
	events::{AnySyncEphemeralRoomEvent, receipt::ReceiptEvent},
	serde::Raw,
};
use serde::{Deserialize, de::IgnoredAny};
use tuwunel_core::{
	Result, err, error, is_equal_to,
	matrix::pdu::PduCount,
	smallvec::SmallVec,
	trace,
	utils::{ReadyExt, TryReadyExt, stream::TryIgnore},
};
use tuwunel_database::{Deserialized, Interfix, Json, KeyBuf, Map, Txn, serialize_key};

use super::{PrivateRead, ThreadKind};

pub(super) struct Data {
	roomuserid_privateread: Arc<Map>,
	roomuserid_lastprivatereadupdate: Arc<Map>,
	roomuserid_privatereadsync: Arc<Map>,
	services: Arc<crate::services::OnceServices>,
	readreceiptid_readreceipt: Arc<Map>,
}

pub(super) type ReceiptItem<'a> = (&'a UserId, u64, Raw<AnySyncEphemeralRoomEvent>);

/// Row shape shared by the active and mirrored private read maps.
type RowKv<'a> = ((&'a RoomId, &'a UserId, &'a str), (u64, Option<u64>));

/// Receipt rows an accepted update replaces.
///
/// A user normally holds one row per thread context; an unthreaded sweep can
/// also catch a pre-MSC3771 row, and that second key spills to the heap.
type Superseded = SmallVec<[KeyBuf; 1]>;

/// Minimal read-back of a stored receipt row.
///
/// Only the content's event ids are read, so the reject path never
/// materializes the receipts themselves. The wire shape is a JSON object,
/// which no set type can express, hence the zero-sized value.
#[derive(Deserialize)]
#[expect(clippy::zero_sized_map_values)]
struct StoredContent {
	content: BTreeMap<OwnedEventId, IgnoredAny>,
}

impl Data {
	pub(super) fn new(args: &crate::Args<'_>) -> Self {
		let db = &args.db;
		Self {
			roomuserid_privateread: db["roomuserid_privateread"].clone(),
			roomuserid_lastprivatereadupdate: db["roomuserid_lastprivatereadupdate"].clone(),
			roomuserid_privatereadsync: db["roomuserid_privatereadsync"].clone(),
			readreceiptid_readreceipt: db["readreceiptid_readreceipt"].clone(),
			services: args.services.clone(),
		}
	}

	/// Stores `event` as the user's receipt for its thread context, reporting
	/// whether it advanced.
	///
	/// A receipt naming the stored event, or an earlier one, is rejected
	/// without allocating a stream position or writing anything. An accepted
	/// receipt replaces every superseded row in one transaction.
	#[inline]
	pub(super) async fn readreceipt_update(
		&self,
		user_id: &UserId,
		room_id: &RoomId,
		event: &ReceiptEvent,
	) -> bool {
		// Remote-supplied content reaches this sink over federation, so an
		// empty receipt is rejected rather than stored as an unreadable row.
		let Some(event_id) = event.content.keys().next() else {
			return false;
		};

		let thread_kind = event_thread_kind(event);
		// MSC3771: storage key suffix is `user_id || 0xFF || thread_kind` so
		// each (user, thread-context) tuple lives in its own row. Pre-MSC3771
		// rows have no kind tail; on an Unthreaded sweep also match the
		// bare-user-id ending so legacy rows are superseded rather than
		// orphaned. Kind tails ("main", `$root`) never end in `@user:host`,
		// so the legacy match cannot collide with thread-aware rows.
		let suffix = serialize_key((user_id, thread_kind))
			.expect("failed to serialize receipt key suffix");

		let user_id_bytes = user_id.as_bytes();
		let legacy_match = thread_kind.is_empty();

		// A bare room-id prefix also matches longer room ids, whose rows sort
		// below ours in reverse iteration and would be reaped by the sweep.
		let room_prefix =
			serialize_key((room_id, Interfix)).expect("failed to serialize receipt room prefix");

		let last_possible_key = (room_id, u64::MAX);
		let (superseded, current) = self
			.readreceiptid_readreceipt
			.rev_stream_from_raw(&last_possible_key)
			.ignore_err()
			.ready_take_while(|(key, _)| key.starts_with(room_prefix.as_slice()))
			.ready_filter_map(|(key, val)| {
				(key.ends_with(suffix.as_slice())
					|| (legacy_match && key.ends_with(user_id_bytes)))
				.then_some((key, val))
			})
			.ready_fold((Superseded::new(), None), |(mut superseded, current), (key, val)| {
				let current = superseded
					.is_empty()
					.then_some(val)
					.and_then(stored_event_id)
					.or(current);

				superseded.push(key.into());

				(superseded, current)
			})
			.await;

		if !self
			.receipt_advanced(current.as_deref(), event_id)
			.await
		{
			return false;
		}

		let count = self.services.globals.next_count();
		let latest_id = (room_id, *count, user_id, thread_kind);

		let mut txn = superseded
			.iter()
			.fold(self.services.db.txn(), |mut txn, key| {
				txn.del_raw(&self.readreceiptid_readreceipt, key);
				txn
			});

		txn.put(&self.readreceiptid_readreceipt, latest_id, Json(event));
		txn.execute();

		true
	}

	/// Whether a receipt for `incoming` supersedes the stored one at
	/// `current`.
	///
	/// An identical event id never advances. A position that does not resolve
	/// to a known PDU falls through to acceptance, so a receipt this server
	/// cannot order is never silently dropped.
	async fn receipt_advanced(&self, current: Option<&EventId>, incoming: &EventId) -> bool {
		match current {
			| None => true,
			| Some(current) if current == incoming => false,
			| Some(current) => {
				let (current, incoming) = join(
					self.services.timeline.get_pdu_count(current),
					self.services.timeline.get_pdu_count(incoming),
				)
				.await;

				position_advances(current.ok(), incoming.ok())
			},
		}
	}

	#[inline]
	pub(super) fn readreceipts_since<'a>(
		&'a self,
		room_id: &'a RoomId,
		since: u64,
		to: Option<u64>,
	) -> impl Stream<Item = ReceiptItem<'_>> + Send + 'a {
		self.readreceipts_since_fallible(room_id, since, to)
			.ignore_err()
	}

	#[inline]
	pub(super) fn readreceipts_since_fallible<'a>(
		&'a self,
		room_id: &'a RoomId,
		since: u64,
		to: Option<u64>,
	) -> impl Stream<Item = Result<ReceiptItem<'_>>> + Send + 'a {
		// 4-tuple key: pre-MSC3771 rows deserialize with `&str` tail empty.
		type Key<'a> = (&'a RoomId, u64, &'a UserId, &'a str);
		type KeyVal<'a> = (Key<'a>, CanonicalJsonObject);

		let after_since = since.saturating_add(1); // +1 so we don't send the event at since
		let first_possible_edu = (room_id, after_since);

		self.readreceiptid_readreceipt
			.stream_from(&first_possible_edu)
			.ready_try_take_while(move |((r, c, ..), _): &KeyVal<'_>| {
				Ok(*r == room_id && to.is_none_or(|to| *c <= to))
			})
			.ready_and_then(move |((_, count, user_id, _), mut json): KeyVal<'_>| {
				json.remove("room_id");

				let event = serde_json::value::to_raw_value(&json)?;

				Ok((user_id, count, Raw::from_json(event)))
			})
	}

	#[inline]
	pub(super) async fn last_receipt_count<'a>(
		&'a self,
		room_id: &'a RoomId,
		upper: Option<u64>,
		user_id: Option<&'a UserId>,
	) -> Result<u64> {
		// 4-tuple key: pre-MSC3771 rows deserialize with `&str` tail empty.
		type Key<'a> = (&'a RoomId, u64, &'a UserId, &'a str);

		let key = (room_id, upper.unwrap_or(u64::MAX));

		self.readreceiptid_readreceipt
			.rev_keys_from(&key)
			.ignore_err()
			.ready_take_while(|(room_id_, ..): &Key<'_>| *room_id_ == room_id)
			.ready_filter(|(_, _, user_id_, _): &Key<'_>| {
				user_id.is_none_or(is_equal_to!(*user_id_))
			})
			.map(|(_, count, ..): Key<'_>| count)
			.boxed()
			.next()
			.await
			.ok_or_else(|| err!(Request(NotFound("No receipts found in room"))))
	}

	/// Sets the private read marker for `(room, user, thread)`, reporting
	/// whether it advanced.
	///
	/// Unthreaded writes use the legacy 2-tuple `(room, user)` key shape
	/// and sweep any pre-existing per-thread rows so the room-wide receipt
	/// supersedes prior thread state. Threaded writes (Main, Thread, custom)
	/// use a 3-tuple `(room, user, thread_kind)` key disjoint from the
	/// legacy row by trailing separator. The sync gate
	/// (`roomuserid_lastprivatereadupdate`) stays 2-tuple and bumps only when
	/// `announce` is set, keeping it a single point query. Announced state is
	/// mirrored separately so notification-only writes cannot alter a sync
	/// snapshot without changing its version.
	#[inline]
	pub(super) async fn private_read_set(
		&self,
		PrivateRead {
			room_id,
			user_id,
			count,
			ts,
			thread,
			announce,
		}: PrivateRead<'_>,
	) -> bool {
		let thread_kind = thread.as_str().unwrap_or_default();

		if self
			.private_read_position(room_id, user_id, thread_kind)
			.await
			.is_ok_and(|(stored, _)| count <= stored)
		{
			return false;
		}

		let reset_sync = if announce && !thread_kind.is_empty() {
			let versions = try_join(
				self.last_privateread_update_fallible(user_id, room_id),
				self.private_read_sync_update_fallible(user_id, room_id),
			)
			.await;

			match versions {
				| Ok((gate, snapshot)) => gate != snapshot,
				| Err(error) => {
					error!(?error, "Failed to inspect the private read sync snapshot.");
					return false;
				},
			}
		} else {
			false
		};

		let mut txn = self
			.sweep_thread_private_reads(
				&self.roomuserid_privateread,
				room_id,
				user_id,
				thread_kind,
				self.services.db.txn(),
			)
			.await;

		if announce && (thread_kind.is_empty() || reset_sync) {
			txn = match self
				.sweep_private_read_sync(room_id, user_id, txn)
				.await
			{
				| Ok(txn) => txn,
				| Err(error) => {
					error!(?error, "Failed to reset the private read sync snapshot.");
					return false;
				},
			};
		}

		// The permit retires the sequence number on drop, so it outlives execute().
		let next_count = announce.then(|| self.services.globals.next_count());
		let ts = u64::from(ts.get());

		if let Some(next_count) = next_count.as_deref() {
			txn.put(&self.roomuserid_lastprivatereadupdate, (room_id, user_id), *next_count);
			txn.put(&self.roomuserid_privatereadsync, (room_id, user_id), *next_count);
			txn.put(
				&self.roomuserid_privatereadsync,
				(room_id, user_id, thread_kind),
				(count, ts),
			);
		}

		// Additive value tail: ts (millis); old bare-count rows read back None.
		match thread_kind.is_empty() {
			| true => txn.put(&self.roomuserid_privateread, (room_id, user_id), (count, ts)),
			| false => txn.put(
				&self.roomuserid_privateread,
				(room_id, user_id, thread_kind),
				(count, ts),
			),
		}

		txn.execute();

		true
	}

	/// Private read position for an exact `(room, user, thread)` context.
	///
	/// An unthreaded context reads the legacy 2-tuple row; a threaded one
	/// reads its own 3-tuple row.
	#[inline]
	async fn private_read_position(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
		thread_kind: &str,
	) -> Result<(u64, Option<u64>)> {
		match thread_kind.is_empty() {
			| true =>
				self.private_read_get_count(room_id, user_id)
					.await,
			| false => self
				.roomuserid_privateread
				.qry(&(room_id, user_id, thread_kind))
				.await
				.deserialized(),
		}
	}

	/// Latest unthreaded (legacy 2-tuple) private read: `(pdu count, receipt ts
	/// millis)`. `ts` is `None` for rows written before the ts tail was added.
	#[inline]
	pub(super) async fn private_read_get_count(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
	) -> Result<(u64, Option<u64>)> {
		let key = (room_id, user_id);
		self.roomuserid_privateread
			.qry(&key)
			.await
			.deserialized()
	}

	#[inline]
	pub(super) async fn private_read_sync_get_count(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
	) -> Result<(u64, Option<u64>)> {
		let key = (room_id, user_id, "");
		self.roomuserid_privatereadsync
			.qry(&key)
			.await
			.deserialized()
	}

	#[inline]
	pub(super) fn private_read_threaded_stream<'a>(
		&'a self,
		room_id: &'a RoomId,
		user_id: &'a UserId,
	) -> impl Stream<Item = (ThreadKind, u64, Option<u64>)> + Send + 'a {
		private_read_row_stream(&self.roomuserid_privateread, room_id, user_id).ignore_err()
	}

	/// Queues deletion of the per-thread private read rows for `(room, user)`.
	///
	/// Only an unthreaded write sweeps, since its room-wide receipt supersedes
	/// prior thread state; a threaded write touches only its own row.
	#[inline]
	async fn sweep_thread_private_reads(
		&self,
		map: &Arc<Map>,
		room_id: &RoomId,
		user_id: &UserId,
		thread_kind: &str,
		txn: Txn,
	) -> Txn {
		if !thread_kind.is_empty() {
			return txn;
		}

		let prefix = (room_id, user_id, Interfix);

		map.keys_prefix_raw(&prefix)
			.ignore_err()
			.ready_fold(txn, |mut txn, key| {
				txn.del_raw(map, key);
				txn
			})
			.await
	}

	#[inline]
	async fn sweep_private_read_sync(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
		txn: Txn,
	) -> Result<Txn> {
		let prefix = (room_id, user_id, Interfix);

		self.roomuserid_privatereadsync
			.keys_prefix_raw(&prefix)
			.ready_try_fold(txn, |mut txn, key| {
				txn.del_raw(&self.roomuserid_privatereadsync, key);
				Ok(txn)
			})
			.await
	}

	#[inline]
	pub(super) fn private_read_sync_stream_fallible<'a>(
		&'a self,
		room_id: &'a RoomId,
		user_id: &'a UserId,
	) -> impl Stream<Item = Result<(ThreadKind, u64, Option<u64>)>> + Send + 'a {
		private_read_row_stream(&self.roomuserid_privatereadsync, room_id, user_id)
	}

	#[inline]
	pub(super) async fn private_read_sync_update_fallible(
		&self,
		user_id: &UserId,
		room_id: &RoomId,
	) -> Result<u64> {
		let key = (room_id, user_id);
		self.roomuserid_privatereadsync
			.qry(&key)
			.await
			.deserialized()
			.or_else(|error| error.is_not_found().then_some(0).ok_or(error))
	}

	#[inline]
	pub(super) async fn last_privateread_update(
		&self,
		user_id: &UserId,
		room_id: &RoomId,
	) -> u64 {
		self.last_privateread_update_fallible(user_id, room_id)
			.await
			.unwrap_or_default()
	}

	#[inline]
	pub(super) async fn last_privateread_update_fallible(
		&self,
		user_id: &UserId,
		room_id: &RoomId,
	) -> Result<u64> {
		let key = (room_id, user_id);
		self.roomuserid_lastprivatereadupdate
			.qry(&key)
			.await
			.deserialized()
			.or_else(|error| error.is_not_found().then_some(0).ok_or(error))
	}

	#[inline]
	pub(super) async fn delete_all_read_receipts(&self, room_id: &RoomId) -> Result {
		let prefix = (room_id, Interfix);

		self.roomuserid_privateread
			.keys_prefix_raw(&prefix)
			.ignore_err()
			.ready_for_each(|key| {
				trace!("Removing key: {key:?}");
				self.roomuserid_privateread.remove(key);
			})
			.await;

		self.roomuserid_lastprivatereadupdate
			.keys_prefix_raw(&prefix)
			.ignore_err()
			.ready_for_each(|key| {
				trace!("Removing key: {key:?}");
				self.roomuserid_lastprivatereadupdate.remove(key);
			})
			.await;

		self.roomuserid_privatereadsync
			.keys_prefix_raw(&prefix)
			.ignore_err()
			.ready_for_each(|key| {
				trace!("Removing key: {key:?}");
				self.roomuserid_privatereadsync.remove(key);
			})
			.await;

		self.readreceiptid_readreceipt
			.keys_prefix_raw(&prefix)
			.ignore_err()
			.ready_for_each(|key| {
				trace!("Removing key: {key:?}");
				self.readreceiptid_readreceipt.remove(key);
			})
			.await;

		Ok(())
	}
}

/// Per-thread marker rows under `(room, user)` in `map`.
///
/// Active markers in the live store and announced markers in the sync mirror
/// share one row shape, keyed by thread kind.
fn private_read_row_stream<'a>(
	map: &'a Arc<Map>,
	room_id: &'a RoomId,
	user_id: &'a UserId,
) -> impl Stream<Item = Result<(ThreadKind, u64, Option<u64>)>> + Send + 'a {
	let prefix = (room_id, user_id, Interfix);

	map.stream_prefix(&prefix)
		.map_ok(|((_, _, kind), (count, ts)): RowKv<'_>| (ThreadKind::from(kind), count, ts))
}

/// Tag string used in the storage key to discriminate receipts per thread.
/// Empty for `Unthreaded`, `"main"` for `Main`, the event-id string for
/// `Thread(...)` (event ids start with `$`, so the values are mutually
/// exclusive). Custom variants reuse their string form; the C/S boundary
/// rejects them, but federation receipts may still carry them through.
///
/// Reads only the first `(event_id, type, user)` triple. All callers
/// build single-entry receipts (one event id, one type, one user); a
/// debug assertion catches future regressions. An entirely empty event
/// or one whose only receipt lacks a thread field falls back to `""`.
///
/// Appended to the receipt-row key as a tolerant trailing field. Pre-
/// MSC3771 rows have no trailing kind; they round-trip as `""`.
fn event_thread_kind(event: &ReceiptEvent) -> &str {
	debug_assert!(
		event
			.content
			.values()
			.all(|by_type| by_type.len() == 1
				&& by_type.values().all(|by_user| by_user.len() == 1))
			&& event.content.len() == 1,
		"receipt event must carry exactly one (event_id, type, user) triple"
	);

	event
		.content
		.values()
		.next()
		.and_then(|by_type| by_type.values().next())
		.and_then(|by_user| by_user.values().next())
		.and_then(|receipt| receipt.thread.as_str())
		.unwrap_or_default()
}

/// First event id named by a stored receipt row.
///
/// `None` when the row does not deserialize, which the caller treats as an
/// unknown position and accepts, replacing the row.
fn stored_event_id(val: &[u8]) -> Option<OwnedEventId> {
	serde_json::from_slice::<StoredContent>(val)
		.ok()?
		.content
		.into_keys()
		.next()
}

/// Whether an incoming receipt position strictly advances the stored one.
///
/// An unresolved position on either side accepts. `PduCount` ordering places
/// backfilled events below normal ones, so no separate branch is needed.
pub(super) fn position_advances(current: Option<PduCount>, incoming: Option<PduCount>) -> bool {
	current
		.zip(incoming)
		.is_none_or(|(current, incoming)| incoming > current)
}
