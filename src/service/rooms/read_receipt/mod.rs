mod data;
#[cfg(test)]
mod tests;

use std::{collections::BTreeMap, sync::Arc};

use futures::{Stream, StreamExt, TryStreamExt};
use ruma::{
	MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId, RoomId, UInt, UserId,
	api::appservice::event::push_events::v1::EphemeralData,
	events::{
		AnySyncEphemeralRoomEvent, SyncEphemeralRoomEvent,
		receipt::{
			Receipt, ReceiptEvent, ReceiptEventContent, ReceiptThread, ReceiptType, Receipts,
		},
	},
	serde::Raw,
};
use serde_json::value::to_raw_value;
use tuwunel_core::{
	Result, debug,
	debug::INFO_SPAN_LEVEL,
	err,
	matrix::{
		Event,
		pdu::{PduCount, PduId, RawPduId},
	},
	smallstr::SmallString,
	smallvec::SmallVec,
	utils::{BoolExt, IterStream},
	warn,
};

use self::data::{Data, ReceiptItem};

/// Private read receipts surfaced by `private_read_get`. One legacy
/// unthreaded row plus zero or more per-thread rows; inline-1 catches the
/// dominant case (a single unthreaded marker) without a heap alloc.
pub type PrivateReadEvents = SmallVec<[Raw<AnySyncEphemeralRoomEvent>; 1]>;

/// Stored thread-kind tag: `""` for `Unthreaded`, `"main"` for `Main`, or
/// the event-id string for `Thread(...)`. v3+ event ids are 44 bytes
/// including the leading `$`; 48 bytes inline matches the project's
/// `StateKey` budget and stays inline for every realistic thread root.
type ThreadKind = SmallString<[u8; 48]>;

/// A private read marker write for one `(room, user, thread)` context.
///
/// `count` is the timeline position the marker addresses and `ts` the receipt
/// timestamp. `announce` opens the sync gate, carrying the marker to the
/// user's other devices; a marker the server writes on the user's behalf
/// leaves it closed.
#[derive(Clone, Copy, Debug)]
pub struct PrivateRead<'a> {
	pub room_id: &'a RoomId,
	pub user_id: &'a UserId,
	pub count: u64,
	pub ts: MilliSecondsSinceUnixEpoch,
	pub thread: &'a ReceiptThread,
	pub announce: bool,
}

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	db: Data,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			db: Data::new(args),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// Replaces the previous read receipt when the incoming one advances.
	///
	/// Returns whether the receipt was stored. A re-posted marker allocates no
	/// stream position, so appservice and federation delivery are both skipped.
	#[tracing::instrument(
		name = "receipt"
		level = INFO_SPAN_LEVEL,
		skip_all,
		fields(
			%room_id,
			%user_id,
			?event.content
		)
	)]
	pub async fn readreceipt_update(
		&self,
		user_id: &UserId,
		room_id: &RoomId,
		event: &ReceiptEvent,
	) -> bool {
		if self
			.db
			.readreceipt_update(user_id, room_id, event)
			.await
			.is_false()
		{
			return false;
		}

		self.services
			.sending
			.send_edu_room_appservices(room_id, |buf| {
				let edu = EphemeralData::Receipt(ReceiptEvent {
					content: event.content.clone(),
					room_id: room_id.to_owned(),
				});

				Ok(serde_json::to_writer(buf, &edu)?)
			})
			.await
			.expect("edu serialization or flush failed");

		if self.services.globals.user_is_local(user_id) {
			self.services
				.sending
				.flush_room(room_id)
				.await
				.expect("room flush failed");
		}

		true
	}

	/// Gets every stored private read receipt for `(room, user)`. Returns
	/// one ephemeral event per stored row (legacy unthreaded plus per-thread
	/// rows). An empty result means no marker is set.
	#[tracing::instrument(skip(self), level = "debug", name = "get_private")]
	pub async fn private_read_get(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
	) -> Result<PrivateReadEvents> {
		let shortroomid = self
			.services
			.short
			.get_shortroomid(room_id)
			.await
			.map_err(|e| {
				err!(Database(warn!(
					"Short room ID does not exist in database for {room_id}: {e}"
				)))
			})?;

		let legacy = self
			.private_read_get_count(room_id, user_id)
			.await
			.ok()
			.map(|(count, ts)| (ThreadKind::new(), count, ts));

		let events = legacy
			.into_iter()
			.stream()
			.chain(
				self.db
					.private_read_threaded_stream(room_id, user_id),
			)
			.filter_map(async |(kind, count, ts)| {
				self.build_private_read_event(shortroomid, count, ts, user_id, &kind)
					.await
			})
			.collect()
			.await;

		Ok(events)
	}

	/// Gets the complete announced private read snapshot for `update` without
	/// suppressing malformed rows.
	///
	/// A snapshot older than the gate predates this durable mirror, so those
	/// rows fall back to the tolerant active-state read they always published
	/// rather than withholding the whole room after an upgrade. A newer
	/// snapshot means a concurrent announce; that fails the bounded room range
	/// so its cursor remains pinned. A marker naming an event that no longer
	/// resolves is skipped rather than failing the range, which would
	/// otherwise repeat on every request and withhold the room indefinitely.
	#[tracing::instrument(skip(self), level = "debug", name = "get_private_fallible")]
	pub async fn private_read_get_fallible(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
		update: u64,
	) -> Result<PrivateReadEvents> {
		let snapshot = self
			.db
			.private_read_sync_update_fallible(user_id, room_id)
			.await?;

		if snapshot < update {
			debug!(%room_id, %user_id, "Serving pre-mirror private read from the active store.");
			return self.private_read_get(room_id, user_id).await;
		}

		if snapshot > update {
			return Err(err!(Database(
				"Private read snapshot advanced while assembling a bounded sync range."
			)));
		}

		let shortroomid = async {
			self.services
				.short
				.get_shortroomid(room_id)
				.await
				.map_err(|e| {
					err!(Database(warn!(
						"Short room ID does not exist in database for {room_id}: {e}"
					)))
				})
		};

		let shortroomid = shortroomid.await?;
		let events = self
			.db
			.private_read_sync_stream_fallible(room_id, user_id)
			.try_filter_map(async |(kind, count, ts)| {
				self.build_private_read_event_skippable(shortroomid, count, ts, user_id, &kind)
					.await
			})
			.try_collect()
			.await?;

		let confirmed = self
			.db
			.private_read_sync_update_fallible(user_id, room_id)
			.await?;

		if confirmed != update {
			return Err(err!(Database(
				"Private read snapshot changed while assembling a bounded sync range."
			)));
		}

		Ok(events)
	}

	/// Builds one announced private read row, skipping a marker whose event
	/// no longer resolves.
	///
	/// The absent case returns `Ok(None)` so the bounded range assembles
	/// without it. Decode failures and invalid timestamps still fail the
	/// range as real inconsistencies.
	async fn build_private_read_event_skippable(
		&self,
		shortroomid: u64,
		count: u64,
		ts: Option<u64>,
		user_id: &UserId,
		thread_kind: &str,
	) -> Result<Option<Raw<AnySyncEphemeralRoomEvent>>> {
		let skip = || {
			debug!(
				count,
				thread_kind,
				%user_id,
				"Skipping a private read marker naming a missing event."
			);

			None
		};

		self.build_private_read_event_fallible(shortroomid, count, ts, user_id, thread_kind)
			.await
			.map(Some)
			.or_else(|error| error.is_not_found().then(skip).ok_or(error))
	}

	async fn build_private_read_event(
		&self,
		shortroomid: u64,
		count: u64,
		ts: Option<u64>,
		user_id: &UserId,
		thread_kind: &str,
	) -> Option<Raw<AnySyncEphemeralRoomEvent>> {
		let thread = thread_kind_to_receipt(thread_kind).unwrap_or(ReceiptThread::Unthreaded);
		let ts = ts
			.and_then(UInt::new)
			.map(MilliSecondsSinceUnixEpoch);

		self.build_private_read_event_from(shortroomid, count, ts, user_id, thread)
			.await
			.ok()
	}

	async fn build_private_read_event_fallible(
		&self,
		shortroomid: u64,
		count: u64,
		ts: Option<u64>,
		user_id: &UserId,
		thread_kind: &str,
	) -> Result<Raw<AnySyncEphemeralRoomEvent>> {
		let thread = thread_kind_to_receipt(thread_kind)?;
		let ts = ts
			.map(|ts| {
				UInt::new(ts)
					.map(MilliSecondsSinceUnixEpoch)
					.ok_or_else(|| err!(Database("Invalid private receipt timestamp {ts}.")))
			})
			.transpose()?;

		self.build_private_read_event_from(shortroomid, count, ts, user_id, thread)
			.await
	}

	async fn build_private_read_event_from(
		&self,
		shortroomid: u64,
		count: u64,
		ts: Option<MilliSecondsSinceUnixEpoch>,
		user_id: &UserId,
		thread: ReceiptThread,
	) -> Result<Raw<AnySyncEphemeralRoomEvent>> {
		let pdu_id: RawPduId = PduId {
			shortroomid,
			count: PduCount::Normal(count),
		}
		.into();
		let pdu = self
			.services
			.timeline
			.get_pdu_from_id(&pdu_id)
			.await?;

		let event_id: OwnedEventId = pdu.event_id().to_owned();
		let user_id: OwnedUserId = user_id.to_owned();
		let content: BTreeMap<OwnedEventId, Receipts> = BTreeMap::from_iter([(
			event_id,
			BTreeMap::from_iter([(
				ReceiptType::ReadPrivate,
				BTreeMap::from_iter([(user_id, Receipt { ts, thread })]),
			)]),
		)]);

		let receipt_event_content = ReceiptEventContent(content);
		let receipt_sync_event = SyncEphemeralRoomEvent { content: receipt_event_content };
		let event = to_raw_value(&receipt_sync_event)?;

		Ok(Raw::from_json(event))
	}

	/// Returns an iterator over the most recent read_receipts in a room that
	/// happened after the event with id `since`.
	#[tracing::instrument(skip(self), level = "debug")]
	pub fn readreceipts_since<'a>(
		&'a self,
		room_id: &'a RoomId,
		since: u64,
		to: Option<u64>,
	) -> impl Stream<Item = ReceiptItem<'_>> + Send + 'a {
		self.db.readreceipts_since(room_id, since, to)
	}

	/// Returns read receipts in a bounded room range without suppressing
	/// failures.
	///
	/// The lower bound is exclusive and the optional upper bound is inclusive.
	/// Cursor, decode, and serialization failures remain in the stream for an
	/// atomic caller to handle.
	#[tracing::instrument(skip(self), level = "debug")]
	pub fn readreceipts_since_fallible<'a>(
		&'a self,
		room_id: &'a RoomId,
		since: u64,
		to: Option<u64>,
	) -> impl Stream<Item = Result<ReceiptItem<'_>>> + Send + 'a {
		self.db
			.readreceipts_since_fallible(room_id, since, to)
	}

	/// Sets a private read marker at PDU `count` for the given thread.
	///
	/// Unthreaded writes supersede prior per-thread rows so the room-wide
	/// receipt subsumes thread state. Returns whether the marker advanced; a
	/// position at or behind the stored one writes nothing.
	#[tracing::instrument(skip(self), level = "debug", name = "set_private")]
	pub async fn private_read_set(&self, private_read: PrivateRead<'_>) -> bool {
		self.db.private_read_set(private_read).await
	}

	/// Returns the private read marker PDU count.
	#[tracing::instrument(
		name = "get_private_count",
		level = "debug",
		skip(self),
		ret(level = "trace")
	)]
	pub async fn private_read_get_count(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
	) -> Result<(u64, Option<u64>)> {
		self.db
			.private_read_get_count(room_id, user_id)
			.await
	}

	/// Returns the announced unthreaded private read marker PDU count.
	#[tracing::instrument(
		name = "get_private_sync_count",
		level = "debug",
		skip(self),
		ret(level = "trace")
	)]
	pub async fn private_read_sync_get_count(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
	) -> Result<(u64, Option<u64>)> {
		self.db
			.private_read_sync_get_count(room_id, user_id)
			.await
	}

	/// Returns the PDU count of the last private read update in this room.
	///
	/// Missing or unreadable update rows return zero for legacy callers.
	/// Bounded sync callers use the fallible variant below so failures retain
	/// the room cursor.
	#[tracing::instrument(
		name = "get_private_last",
		level = "debug",
		skip(self),
		ret(level = "trace")
	)]
	pub async fn last_privateread_update(&self, user_id: &UserId, room_id: &RoomId) -> u64 {
		self.db
			.last_privateread_update(user_id, room_id)
			.await
	}

	/// Returns the bounded-sync token for the last private read update.
	///
	/// A missing token is returned as zero. Database and decode failures are
	/// preserved so a caller can retain its room cursor and retry the complete
	/// range.
	#[tracing::instrument(
		name = "get_private_last_fallible",
		level = "debug",
		skip(self),
		ret(level = "trace")
	)]
	pub async fn last_privateread_update_fallible(
		&self,
		user_id: &UserId,
		room_id: &RoomId,
	) -> Result<u64> {
		self.db
			.last_privateread_update_fallible(user_id, room_id)
			.await
	}

	pub async fn delete_all_read_receipts(&self, room_id: &RoomId) -> Result {
		self.db.delete_all_read_receipts(room_id).await
	}
}

/// Reverse of `ReceiptThread::as_str`: parse a stored thread tag into the
/// enum. Empty string maps to `Unthreaded`; `"main"` to `Main`; all other
/// values must be valid event IDs.
fn thread_kind_to_receipt(thread_kind: &str) -> Result<ReceiptThread> {
	match thread_kind {
		| "" => Ok(ReceiptThread::Unthreaded),
		| "main" => Ok(ReceiptThread::Main),
		| _ => OwnedEventId::try_from(thread_kind)
			.map(ReceiptThread::Thread)
			.map_err(|error| err!(Database("Invalid private receipt thread: {error}"))),
	}
}

/// Packs read receipts into one sync event without suppressing malformed input.
///
/// Every input must deserialize as a receipt event. The caller receives any
/// parse or serialization failure and can retain the bounded room cursor
/// instead of publishing a partial event.
pub fn pack_receipts_fallible<I>(
	mut receipts: I,
) -> Result<Raw<SyncEphemeralRoomEvent<ReceiptEventContent>>>
where
	I: Iterator<Item = Raw<AnySyncEphemeralRoomEvent>>,
{
	let json = receipts.try_fold(BTreeMap::new(), |mut json, value| -> Result<_> {
		let value = serde_json::from_str::<SyncEphemeralRoomEvent<ReceiptEventContent>>(
			value.json().get(),
		)?;

		for (event, receipt) in value.content {
			json.insert(event, receipt);
		}

		Ok(json)
	})?;

	let content = ReceiptEventContent(json);
	let event = to_raw_value(&SyncEphemeralRoomEvent { content })?;

	Ok(Raw::from_json(event))
}
