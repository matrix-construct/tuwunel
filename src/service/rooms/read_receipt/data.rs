use std::sync::Arc;

use futures::{Stream, StreamExt};
use ruma::{
	CanonicalJsonObject, RoomId, UserId,
	events::{
		AnySyncEphemeralRoomEvent,
		receipt::{ReceiptEvent, ReceiptThread},
	},
	serde::Raw,
};
use tuwunel_core::{
	Result, err, is_equal_to, trace,
	utils::{ReadyExt, stream::TryIgnore},
};
use tuwunel_database::{Deserialized, Interfix, Json, Map, serialize_key};

use super::ThreadKind;

pub(super) struct Data {
	roomuserid_privateread: Arc<Map>,
	roomuserid_lastprivatereadupdate: Arc<Map>,
	services: Arc<crate::services::OnceServices>,
	readreceiptid_readreceipt: Arc<Map>,
}

pub(super) type ReceiptItem<'a> = (&'a UserId, u64, Raw<AnySyncEphemeralRoomEvent>);

impl Data {
	pub(super) fn new(args: &crate::Args<'_>) -> Self {
		let db = &args.db;
		Self {
			roomuserid_privateread: db["roomuserid_privateread"].clone(),
			roomuserid_lastprivatereadupdate: db["roomuserid_lastprivatereadupdate"].clone(),
			readreceiptid_readreceipt: db["readreceiptid_readreceipt"].clone(),
			services: args.services.clone(),
		}
	}

	#[inline]
	pub(super) async fn readreceipt_update(
		&self,
		user_id: &UserId,
		room_id: &RoomId,
		event: &ReceiptEvent,
	) {
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

		let last_possible_key = (room_id, u64::MAX);
		self.readreceiptid_readreceipt
			.rev_keys_from_raw(&last_possible_key)
			.ignore_err()
			.ready_take_while(|key| key.starts_with(room_id.as_bytes()))
			.ready_filter_map(|key| {
				(key.ends_with(suffix.as_slice())
					|| (legacy_match && key.ends_with(user_id_bytes)))
				.then_some(key)
			})
			.ready_for_each(|key| self.readreceiptid_readreceipt.del(key))
			.await;

		let count = self.services.globals.next_count();
		let latest_id = (room_id, *count, user_id, thread_kind);
		self.readreceiptid_readreceipt
			.put(latest_id, Json(event));
	}

	#[inline]
	pub(super) fn readreceipts_since<'a>(
		&'a self,
		room_id: &'a RoomId,
		since: u64,
		to: Option<u64>,
	) -> impl Stream<Item = ReceiptItem<'_>> + Send + 'a {
		// 4-tuple key: pre-MSC3771 rows deserialize with `&str` tail empty.
		type Key<'a> = (&'a RoomId, u64, &'a UserId, &'a str);
		type KeyVal<'a> = (Key<'a>, CanonicalJsonObject);

		let after_since = since.saturating_add(1); // +1 so we don't send the event at since
		let first_possible_edu = (room_id, after_since);

		self.readreceiptid_readreceipt
			.stream_from(&first_possible_edu)
			.ignore_err()
			.ready_take_while(move |((r, c, ..), _): &KeyVal<'_>| {
				*r == room_id && to.is_none_or(|to| *c <= to)
			})
			.map(move |((_, count, user_id, _), mut json): KeyVal<'_>| {
				json.remove("room_id");

				let event = serde_json::value::to_raw_value(&json)?;

				Ok((user_id, count, Raw::from_json(event)))
			})
			.ignore_err()
	}

	#[inline]
	pub(super) async fn last_receipt_count<'a>(
		&'a self,
		room_id: &'a RoomId,
		since: Option<u64>,
		user_id: Option<&'a UserId>,
	) -> Result<u64> {
		// 4-tuple key: pre-MSC3771 rows deserialize with `&str` tail empty.
		type Key<'a> = (&'a RoomId, u64, &'a UserId, &'a str);

		let key = (room_id, u64::MAX);
		self.readreceiptid_readreceipt
			.rev_keys_prefix(&key)
			.ignore_err()
			.ready_take_while(|(_, c, u, _): &Key<'_>| {
				since.is_none_or(|since| since > *c) && user_id.is_none_or(is_equal_to!(*u))
			})
			.map(|(_, c, ..): Key<'_>| c)
			.boxed()
			.next()
			.await
			.ok_or_else(|| err!(Request(NotFound("No receipts found in room"))))
	}

	/// Sets the private read marker for `(room, user, thread)`.
	///
	/// Unthreaded writes use the legacy 2-tuple `(room, user)` key shape
	/// and sweep any pre-existing per-thread rows so the room-wide receipt
	/// supersedes prior thread state. Threaded writes (Main, Thread, custom)
	/// use a 3-tuple `(room, user, thread_kind)` key disjoint from the
	/// legacy row by trailing separator. `roomuserid_lastprivatereadupdate`
	/// stays 2-tuple and is bumped on every write so sync gating remains a
	/// single point query.
	#[inline]
	pub(super) async fn private_read_set(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
		pdu_count: u64,
		ts: u64,
		thread: &ReceiptThread,
	) {
		let next_count = self.services.globals.next_count();

		let lastupdate_key = (room_id, user_id);
		self.roomuserid_lastprivatereadupdate
			.put(lastupdate_key, *next_count);

		// Additive value tail: ts (millis); old bare-count rows read back None.
		match thread.as_str() {
			| Some(thread_kind) if !thread_kind.is_empty() => {
				let key = (room_id, user_id, thread_kind);
				self.roomuserid_privateread
					.put(key, (pdu_count, ts));
			},
			| _ => {
				self.clear_thread_private_reads(room_id, user_id)
					.await;

				let key = (room_id, user_id);
				self.roomuserid_privateread
					.put(key, (pdu_count, ts));
			},
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

	/// Stream of `(thread_kind, pdu_count)` for the per-thread (3-tuple)
	/// private read rows for this `(room, user)`. The legacy 2-tuple row is
	/// excluded by the trailing-separator prefix; query it via
	/// `private_read_get_count`.
	#[inline]
	pub(super) fn private_read_threaded_stream<'a>(
		&'a self,
		room_id: &'a RoomId,
		user_id: &'a UserId,
	) -> impl Stream<Item = (ThreadKind, u64, Option<u64>)> + Send + 'a {
		type ThreadKv<'a> = ((&'a RoomId, &'a UserId, &'a str), (u64, Option<u64>));

		let prefix = (room_id, user_id, Interfix);
		self.roomuserid_privateread
			.stream_prefix(&prefix)
			.ignore_err()
			.map(|((_, _, kind), (count, ts)): ThreadKv<'_>| (ThreadKind::from(kind), count, ts))
	}

	#[inline]
	async fn clear_thread_private_reads(&self, room_id: &RoomId, user_id: &UserId) {
		let prefix = (room_id, user_id, Interfix);
		self.roomuserid_privateread
			.keys_prefix_raw(&prefix)
			.ignore_err()
			.ready_for_each(|key| {
				self.roomuserid_privateread.remove(key);
			})
			.await;
	}

	#[inline]
	pub(super) async fn last_privateread_update(
		&self,
		user_id: &UserId,
		room_id: &RoomId,
	) -> u64 {
		let key = (room_id, user_id);
		self.roomuserid_lastprivatereadupdate
			.qry(&key)
			.await
			.deserialized()
			.unwrap_or(0)
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
