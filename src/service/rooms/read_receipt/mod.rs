mod data;
#[cfg(test)]
mod tests;

use std::{collections::BTreeMap, sync::Arc};

use futures::{Stream, StreamExt};
use ruma::{
	EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UInt,
	UserId,
	api::appservice::event::push_events::v1::EphemeralData,
	events::{
		AnySyncEphemeralRoomEvent, SyncEphemeralRoomEvent,
		receipt::{
			Receipt, ReceiptEvent, ReceiptEventContent, ReceiptThread, ReceiptType, Receipts,
		},
	},
	serde::Raw,
};
use tuwunel_core::{
	Result, debug, err,
	matrix::{
		Event,
		pdu::{PduCount, PduId, RawPduId},
	},
	smallstr::SmallString,
	smallvec::SmallVec,
	trace,
	utils::{IterStream, MutexMap},
	warn,
};

use self::data::{Data, ReceiptItem, event_thread_kind};

/// Private read receipts surfaced by `private_read_get`. One legacy
/// unthreaded row plus zero or more per-thread rows; inline-1 catches the
/// dominant case (a single unthreaded marker) without a heap alloc.
pub type PrivateReadEvents = SmallVec<[Raw<AnySyncEphemeralRoomEvent>; 1]>;

/// Stored thread-kind tag: `""` for `Unthreaded`, `"main"` for `Main`, or
/// the event-id string for `Thread(...)`. v3+ event ids are 44 bytes
/// including the leading `$`; 48 bytes inline matches the project's
/// `StateKey` budget and stays inline for every realistic thread root.
type ThreadKind = SmallString<[u8; 48]>;
type ReceiptMutexKey = (OwnedRoomId, OwnedUserId, ThreadKind);

#[derive(Clone, Copy)]
enum NotificationReset {
	None,
	OnAdvance,
}

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	db: Data,
	update_mutex: MutexMap<ReceiptMutexKey, ()>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			db: Data::new(args),
			update_mutex: MutexMap::new(),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// Advances a public read receipt within its room/user/thread context.
	///
	/// A receipt for the same or an older known timeline position is ignored.
	/// Distinct event IDs with unavailable ordering information are accepted
	/// for compatibility with receipts whose events are not locally known.
	/// This method does not alter local notification counts; client API
	/// receipts must use [`Self::client_readreceipt_update`].
	#[tracing::instrument(skip(self), level = "debug", name = "set_receipt")]
	pub async fn readreceipt_update(
		&self,
		user_id: &UserId,
		room_id: &RoomId,
		event: &ReceiptEvent,
	) {
		self.readreceipt_update_inner(user_id, room_id, event, NotificationReset::None)
			.await
	}

	/// Advances a receipt submitted through the client API.
	///
	/// Notification counts are reset after the advancement decision but before
	/// the accepted receipt becomes visible. Set `notifications_already_reset`
	/// when a private receipt in the same client request performed the reset
	/// before publishing its own stream position.
	#[tracing::instrument(skip(self), level = "debug", name = "set_client_receipt")]
	pub async fn client_readreceipt_update(
		&self,
		user_id: &UserId,
		room_id: &RoomId,
		event: &ReceiptEvent,
		notifications_already_reset: bool,
	) {
		let notification_reset = if notifications_already_reset {
			NotificationReset::None
		} else {
			NotificationReset::OnAdvance
		};

		self.readreceipt_update_inner(user_id, room_id, event, notification_reset)
			.await;
	}

	async fn readreceipt_update_inner(
		&self,
		user_id: &UserId,
		room_id: &RoomId,
		event: &ReceiptEvent,
		notification_reset: NotificationReset,
	) {
		let thread_kind = event_thread_kind(event);
		let mutex_key = (room_id.to_owned(), user_id.to_owned(), ThreadKind::from(thread_kind));
		let _guard = self.update_mutex.lock(&mutex_key).await;
		let event_id = event
			.content
			.keys()
			.next()
			.expect("receipt event must carry an event id");
		let current_event_id = self
			.db
			.current_receipt_event_id(user_id, room_id, thread_kind)
			.await;

		let advances = match current_event_id.as_deref() {
			| Some(current_event_id) if current_event_id != event_id => {
				let (current_count, target_count) = futures::future::join(
					self.services
						.timeline
						.get_pdu_count(current_event_id),
					self.services.timeline.get_pdu_count(event_id),
				)
				.await;

				receipt_advances(
					Some(current_event_id),
					event_id,
					current_count.ok(),
					target_count.ok(),
				)
			},
			| current_event_id => receipt_advances(current_event_id, event_id, None, None),
		};

		if !advances {
			return;
		}

		if matches!(notification_reset, NotificationReset::OnAdvance) {
			self.services
				.pusher
				.reset_notification_counts_for_thread(
					user_id,
					room_id,
					&thread_kind_to_receipt(thread_kind),
				)
				.await;
		}

		// update local
		self.db
			.readreceipt_update(user_id, room_id, event)
			.await;

		// update appservices
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

		// update federation
		if self.services.globals.user_is_local(user_id) {
			self.services
				.sending
				.flush_room(room_id)
				.await
				.expect("room flush failed");
		}
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

	async fn build_private_read_event(
		&self,
		shortroomid: u64,
		count: u64,
		ts: Option<u64>,
		user_id: &UserId,
		thread_kind: &str,
	) -> Option<Raw<AnySyncEphemeralRoomEvent>> {
		let pdu_id: RawPduId = PduId {
			shortroomid,
			count: PduCount::Normal(count),
		}
		.into();
		let pdu = self
			.services
			.timeline
			.get_pdu_from_id(&pdu_id)
			.await
			.ok()?;

		let thread = thread_kind_to_receipt(thread_kind);
		let ts = ts
			.and_then(UInt::new)
			.map(MilliSecondsSinceUnixEpoch);

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
		let event = serde_json::value::to_raw_value(&receipt_sync_event)
			.expect("receipt created manually");

		Some(Raw::from_json(event))
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

	/// Sets a private read marker at PDU `count` for the given thread.
	/// Unthreaded writes supersede prior per-thread rows so the room-wide
	/// receipt subsumes thread state.
	#[tracing::instrument(skip(self), level = "debug", name = "set_private")]
	pub async fn private_read_set(
		&self,
		room_id: &RoomId,
		user_id: &UserId,
		count: u64,
		ts: MilliSecondsSinceUnixEpoch,
		thread: &ReceiptThread,
	) {
		self.db
			.private_read_set(room_id, user_id, count, u64::from(ts.get()), thread)
			.await;
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

	/// Returns the PDU count of the last typing update in this room.
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

	#[tracing::instrument(
		name = "get_receipt_last",
		level = "debug",
		skip(self),
		ret(level = "trace")
	)]
	pub async fn last_receipt_count(
		&self,
		room_id: &RoomId,
		user_id: Option<&UserId>,
		since: Option<u64>,
	) -> Result<u64> {
		self.db
			.last_receipt_count(room_id, since, user_id)
			.await
	}

	pub async fn delete_all_read_receipts(&self, room_id: &RoomId) -> Result {
		self.db.delete_all_read_receipts(room_id).await
	}
}

fn receipt_advances(
	current_event_id: Option<&EventId>,
	target_event_id: &EventId,
	current_count: Option<PduCount>,
	target_count: Option<PduCount>,
) -> bool {
	if current_event_id.is_some_and(|current| current == target_event_id) {
		return false;
	}

	match (current_count, target_count) {
		| (Some(current), Some(target)) => target > current,
		| _ => true,
	}
}

/// Reverse of `ReceiptThread::as_str`: parse a stored thread tag into the
/// enum. Empty string maps to `Unthreaded`; `"main"` to `Main`; anything
/// starting with `$` to `Thread(event_id)` if parseable.
fn thread_kind_to_receipt(thread_kind: &str) -> ReceiptThread {
	match thread_kind {
		| "" => ReceiptThread::Unthreaded,
		| "main" => ReceiptThread::Main,
		| _ => OwnedEventId::try_from(thread_kind)
			.map(ReceiptThread::Thread)
			.unwrap_or(ReceiptThread::Unthreaded),
	}
}

#[must_use]
pub fn pack_receipts<I>(receipts: I) -> Raw<SyncEphemeralRoomEvent<ReceiptEventContent>>
where
	I: Iterator<Item = Raw<AnySyncEphemeralRoomEvent>>,
{
	let mut json = BTreeMap::new();
	for value in receipts {
		let receipt = serde_json::from_str::<SyncEphemeralRoomEvent<ReceiptEventContent>>(
			value.json().get(),
		);
		match receipt {
			| Ok(value) =>
				for (event, receipt) in value.content {
					json.insert(event, receipt);
				},
			| _ => {
				debug!("failed to parse receipt: {:?}", receipt);
			},
		}
	}

	let content = ReceiptEventContent::from_iter(json);

	trace!(?content);
	Raw::from_json(
		serde_json::value::to_raw_value(&SyncEphemeralRoomEvent { content })
			.expect("received valid json"),
	)
}
