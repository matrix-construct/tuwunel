//! MSC4354 sticky events: an index of the events still sticky in each room.
//!
//! Each accepted sticky event gets one row, keyed by its timeline position and
//! valued with the instant it expires. Readers take the value as a prefilter
//! and re-check the event itself, so a row outliving its event is harmless,
//! and an hourly sweep deletes expired rows blind. Sticky events withheld by
//! a soft failure are weighed again as room state changes (`withheld`).

mod withheld;

use std::{
	collections::BTreeSet,
	iter::once,
	sync::{Arc, Mutex as StdMutex},
};

use async_trait::async_trait;
use futures::{Stream, StreamExt, TryStreamExt};
use loole::{Receiver, Sender, unbounded};
use ruma::{EventId, OwnedEventId, OwnedRoomId, RoomId, ServerName};
use tuwunel_core::{
	Result, implement,
	utils::{
		BoolExt, IterStream,
		stream::{ReadyExt, TryIgnore, TryReadyExt, WidebandExt},
		time::now_millis,
		u64_from_bytes,
	},
	warn,
};
use tuwunel_database::{Interfix, Map, Txn};
use tuwunel_matrix::{
	Event,
	pdu::{PduCount, PduEvent, PduId, RawPduId},
};

use crate::{rooms::short::ShortRoomId, service::make_name};

/// Indexes the sticky events of every room by timeline position.
///
/// Each row maps an appended event's position to its expiry, staged in the
/// append's transaction and swept once it lapses. The service also weighs
/// again the sticky events only a room's current state refused.
pub struct Service {
	db: Data,
	services: Arc<crate::services::OnceServices>,

	/// Rooms whose installed state changed, for the withheld-event worker.
	queue: (Sender<OwnedRoomId>, Receiver<OwnedRoomId>),

	/// Withheld events the worker is weighing now, which it promotes only
	/// while they are still sticky.
	reevaluating: StdMutex<BTreeSet<OwnedEventId>>,
}

struct Data {
	pduid_stickyuntil: Arc<Map>,
	roomid_softfailedsticky: Arc<Map>,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				pduid_stickyuntil: args.db["pduid_stickyuntil"].clone(),
				roomid_softfailedsticky: args.db["roomid_softfailedsticky"].clone(),
			},
			services: args.services.clone(),
			queue: unbounded(),
			reevaluating: BTreeSet::new().into(),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		if self.services.db.is_read_only() {
			return Ok(()); // Weighing an event again writes its outcome.
		}

		self.reevaluate_worker().await;

		Ok(())
	}

	async fn interrupt(&self) {
		let (sender, _) = &self.queue;

		if !sender.is_closed() {
			sender.close();
		}
	}

	fn name(&self) -> &str { make_name(module_path!()) }
}

/// Stages the index row of an event being appended, when it is sticky now.
///
/// The row joins the transaction that stores the event, so it exists before
/// the event's count retires and every sync range covering that count finds
/// it.
#[implement(Service)]
pub(crate) fn stage(&self, txn: &mut Txn, pdu_id: &RawPduId, pdu: &PduEvent) {
	if let Some(until) = pdu.sticky_until(now_millis()) {
		txn.insert_raw(&self.db.pduid_stickyuntil, pdu_id, until.to_be_bytes());
	}
}

/// Deletes an event's index row blind, as redaction takes its sticky object.
///
/// No read precedes the delete, since an event with no row is already the
/// outcome it seeks.
#[implement(Service)]
#[inline]
pub(crate) fn unindex(&self, pdu_id: &RawPduId) { self.db.pduid_stickyuntil.remove(pdu_id); }

/// Streams a room's events that are sticky at `now`, in timeline order, with
/// counts after `since` and at or before `through`.
///
/// Each row is re-checked against its event, so an expired, redacted or
/// purged event is skipped. An unreadable row or event is logged and skipped.
/// Counts compare signed, so an unbounded read passes `PduCount::max()`,
/// never `u64::MAX`, which compares below every count.
#[implement(Service)]
pub fn unexpired(
	&self,
	shortroomid: ShortRoomId,
	since: u64,
	through: u64,
	now: u64,
) -> impl Stream<Item = (PduCount, PduEvent)> + Send + '_ {
	let prefix = shortroomid.to_be_bytes();
	let through = PduCount::Normal(through);
	let start: RawPduId = PduId {
		shortroomid,
		count: PduCount::Normal(since.saturating_add(1)),
	}
	.into();

	self.db
		.pduid_stickyuntil
		.raw_stream_from(&start)
		.ready_try_take_while(move |(key, _)| {
			let in_range =
				RawPduId::try_from_bytes(key).is_some_and(|pdu_id| pdu_id.pdu_count() <= through);

			Ok(key.starts_with(&prefix) && in_range)
		})
		.ready_try_filter_map(move |(key, val)| {
			let live = u64_from_bytes(val)? > now;

			Ok(live.then(|| RawPduId::from(key)))
		})
		.inspect_err(move |error| warn!(%shortroomid, %error, "Unreadable sticky index row"))
		.ignore_err()
		.wide_filter_map(move |pdu_id| self.load_unexpired(pdu_id, now))
}

/// Loads an indexed event while it is still sticky at `now`.
///
/// A purged event is skipped quietly; any other read error is logged.
#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
async fn load_unexpired(&self, pdu_id: RawPduId, now: u64) -> Option<(PduCount, PduEvent)> {
	self.services
		.timeline
		.get_pdu_from_id(&pdu_id)
		.await
		.inspect_err(|error| {
			if !error.is_not_found() {
				warn!(?pdu_id, %error, "Unreadable sticky event");
			}
		})
		.ok()
		.filter(|pdu| pdu.sticky_until(now).is_some())
		.map(|pdu| (pdu_id.pdu_count(), pdu))
}

/// Whether an accepted event has an index row still unexpired at `now`.
///
/// Only an event appended to the timeline has a row, so an outlier, a
/// soft-failed event and one dated beyond the future cutoff never pass. A row
/// can outlive its event's redaction, so callers recheck the event itself.
#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
pub async fn indexed_unexpired(&self, event_id: &EventId, now: u64) -> bool {
	let Ok(pdu_id) = self.services.timeline.get_pdu_id(event_id).await else {
		return false;
	};

	self.db
		.pduid_stickyuntil
		.get(&pdu_id)
		.await
		.is_ok_and(|until| u64_from_bytes(&until).is_ok_and(|until| until > now))
}

/// Drops the sticky object from a copy of an event fetched by its id unless the
/// event was accepted into the timeline (MSC4354).
///
/// An event held outside it, whether soft-failed, refused by a policy server
/// or never weighed, was never delivered as sticky, so serving its object
/// would present it as live; clients derive a remaining time from the object
/// alone. A failed lookup reads as not accepted.
#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
pub async fn strip_unaccepted(&self, pdu: PduEvent) -> PduEvent {
	let accepted = pdu.sticky.is_none()
		|| self
			.services
			.timeline
			.non_outlier_pdu_exists(pdu.event_id())
			.await
			.is_ok();

	match accepted {
		| true => pdu,
		| false => PduEvent { sticky: None, ..pdu },
	}
}

/// Queues a room's unexpired local sticky events for a server that has just
/// joined it (MSC4354).
///
/// An erased sender's events are left out. Queueing an event the server
/// already has queued is harmless, since the queue key is the server and the
/// event; a failure to queue one is logged and the rest still go.
#[implement(Service)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		%room_id,
		%server,
	),
)]
pub async fn send_to_joined_server(&self, room_id: &RoomId, server: &ServerName) {
	let Some(shortroomid) = self.shortroomid(room_id).await else {
		return;
	};

	// Queued one at a time, so the server receives them in timeline order.
	self.unexpired(shortroomid, 0, PduCount::max().into_unsigned(), now_millis())
		.ready_filter(|(_, pdu)| self.services.globals.user_is_local(pdu.sender()))
		.wide_filter_map(async |(count, pdu)| {
			self.services
				.users
				.is_erased(pdu.sender())
				.await
				.is_false()
				.then(|| RawPduId::from(PduId { shortroomid, count }))
		})
		.for_each(async |pdu_id| {
			self.services
				.sending
				.send_pdu_servers(once(server).stream(), &pdu_id)
				.await
				.inspect_err(|error| {
					warn!(%room_id, %server, ?pdu_id, %error, "Failed to queue a sticky event");
				})
				.ok();
		})
		.await;
}

/// Streams the timeline positions of every indexed event still sticky at
/// `now`, read from the index alone.
///
/// A redacted or purged event may still appear; an unreadable row is skipped.
#[implement(Service)]
pub fn unexpired_ids(&self, now: u64) -> impl Stream<Item = RawPduId> + Send + '_ {
	self.db
		.pduid_stickyuntil
		.raw_stream()
		.ignore_err()
		.ready_filter(move |(_, val)| u64_from_bytes(val).is_ok_and(|until| until > now))
		.map(|(key, _)| RawPduId::from(key))
}

/// Deletes a room's rows from the index and from the withheld events, as the
/// room is purged.
///
/// The caller holds the room's state lock, as every writer of either column
/// does, so no row for the room lands after this.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub async fn delete_room(&self, room_id: &RoomId) {
	let Some(shortroomid) = self.shortroomid(room_id).await else {
		return;
	};

	del_raw_prefix(&self.db.pduid_stickyuntil, shortroomid.to_be_bytes()).await;

	self.db
		.roomid_softfailedsticky
		.del_prefix(&(shortroomid, Interfix))
		.await;
}

/// Deletes the expired rows of the index and of the withheld events,
/// returning how many went.
///
/// The value alone decides and the delete is blind, so a row whose event was
/// redacted or purged earlier goes the same way once its time passes. An
/// unreadable value counts as expired, and a read-only database is left alone.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub async fn sweep(&self) -> usize {
	if self.services.db.is_read_only() {
		return 0;
	}

	let now = now_millis();
	let index = sweep_expired(&self.db.pduid_stickyuntil, now).await;
	let withheld = sweep_expired(&self.db.roomid_softfailedsticky, now).await;

	index.saturating_add(withheld)
}

#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
async fn shortroomid(&self, room_id: &RoomId) -> Option<ShortRoomId> {
	self.services
		.short
		.get_shortroomid(room_id)
		.await
		.inspect_err(|error| warn!(%room_id, %error, "Unreadable room for sticky events"))
		.ok()
}

#[tracing::instrument(level = "trace", skip_all)]
async fn del_raw_prefix(map: &Arc<Map>, prefix: [u8; 8]) {
	map.raw_keys_prefix(&prefix)
		.ignore_err()
		.ready_for_each(|key| map.remove(key))
		.await;
}

#[tracing::instrument(level = "debug", skip_all)]
async fn sweep_expired(map: &Arc<Map>, now: u64) -> usize {
	map.raw_stream()
		.ignore_err()
		.ready_filter(|(_, val)| {
			u64_from_bytes(val)
				.is_ok_and(|until| until > now)
				.is_false()
		})
		.ready_fold(0_usize, |count, (key, _)| {
			map.remove(key);
			count.saturating_add(1)
		})
		.await
}
