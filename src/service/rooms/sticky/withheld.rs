//! MSC4354: soft-failed sticky events weighed again when room state changes.
//!
//! A sticky event refused only by the room's current state gets a candidate
//! row, valued with its expiry. Each change to a room's installed state queues
//! the room for the worker, which runs its unexpired candidates through the
//! incoming-event pipeline again. A redaction of an event outside the timeline
//! leaves a marker row, and the upgrade keeps a marked event withheld.

use std::{convert::identity, iter::once};

use futures::{Stream, StreamExt};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId, OwnedRoomId, OwnedServerName,
	RoomId, RoomVersionId, UserId,
};
use tuwunel_core::{
	debug, debug_warn, implement,
	smallvec::SmallVec,
	utils::{
		IterStream, ReadyExt,
		stream::{TryIgnore, WidebandExt, automatic_width},
		time::now_millis,
		u64_from_bytes,
	},
	warn,
};
use tuwunel_database::Interfix;
use tuwunel_matrix::{
	Event, PduEvent, event::STICKY_DURATION_MAX, pdu::into_outgoing_federation,
};

use super::Service;
use crate::rooms::short::ShortRoomId;

/// A row's key: the room, the event and the row's kind.
///
/// The room leads, so the worker reads a room's rows as one prefix.
type Row<'a> = (ShortRoomId, &'a EventId, u8);

/// Rooms queued for the worker.
///
/// Usually only the room whose state just changed, so one fits inline.
type Rooms = SmallVec<[OwnedRoomId; 1]>;

/// How long a redaction marker lasts, past the expiry of any event still
/// sticky when it is written.
///
/// It therefore outlives any candidate it guards, and the sweep deletes it as
/// it deletes any lapsed row.
const REDACTED_FOR: u64 = 2 * STICKY_DURATION_MAX;

/// What a row records about its event.
///
/// The kind is the key's last byte, so one event can hold a candidate row and
/// a redaction marker at once.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
	/// Only the room's current state refused the event; valued with its expiry.
	Candidate = 0,

	/// A redaction arrived while the event was outside the timeline.
	Redacted = 1,
}

impl From<Kind> for u8 {
	fn from(kind: Kind) -> Self {
		match kind {
			| Kind::Candidate => 0,
			| Kind::Redacted => 1,
		}
	}
}

/// Records a sticky event that only the room's current state refused, so a
/// change to that state weighs it again.
///
/// An event no longer sticky is not recorded. The caller holds the room's
/// state lock.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn withhold(&self, room_id: &RoomId, pdu: &PduEvent) {
	let Some(until) = pdu.sticky_until(now_millis()) else {
		return;
	};

	let Some(shortroomid) = self.shortroomid(room_id).await else {
		return;
	};

	let candidate: Row<'_> = (shortroomid, pdu.event_id(), Kind::Candidate.into());

	self.db
		.roomid_softfailedsticky
		.put(candidate, until);
}

/// Records a redaction of an event outside the timeline, so a sticky event
/// withheld at the time is never promoted.
///
/// The marker is written blind for any such target, and expires past every
/// event still sticky when it is written.
#[implement(Service)]
pub(crate) fn mark_redacted(&self, shortroomid: ShortRoomId, event_id: &EventId) {
	let until = now_millis().saturating_add(REDACTED_FOR);
	let marker: Row<'_> = (shortroomid, event_id, Kind::Redacted.into());

	self.db.roomid_softfailedsticky.put(marker, until);
}

/// Whether a sticky event leaving the soft-failed set must stay withheld.
///
/// It must when it was redacted while outside the timeline, or when the worker
/// is weighing it and it is no longer sticky; its candidate row then goes. An
/// unreadable marker refuses this promotion and keeps the candidate for the
/// next change. The caller holds the room's state lock, as the marker's writer
/// does.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn promotion_refused(&self, room_id: &RoomId, pdu: &PduEvent) -> bool {
	if pdu.sticky().is_none() {
		return false;
	}

	let Some(shortroomid) = self.shortroomid(room_id).await else {
		return true;
	};

	let marker: Row<'_> = (shortroomid, pdu.event_id(), Kind::Redacted.into());
	let redacted = self
		.db
		.roomid_softfailedsticky
		.try_contains(&marker)
		.await
		.inspect_err(
			|error| warn!(%room_id, event_id = %pdu.event_id(), %error, "Unreadable redaction marker"),
		);

	let Ok(redacted) = redacted else {
		return true;
	};

	let lapsed = self.is_reevaluating(pdu.event_id()) && pdu.sticky_until(now_millis()).is_none();
	let refused = redacted || lapsed;

	if refused {
		let candidate: Row<'_> = (shortroomid, pdu.event_id(), Kind::Candidate.into());

		self.db.roomid_softfailedsticky.del(candidate);
	}

	refused
}

/// Whether an event is withheld only by the room's current state, as a
/// candidate still unexpired at `now` with no redaction recorded against it.
///
/// No policy or spam refusal leaves a candidate, so a joined server may see
/// such an event as it sees an accepted one. An unreadable marker counts as a
/// redaction.
#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) async fn withheld_unexpired(
	&self,
	room_id: &RoomId,
	event_id: &EventId,
	now: u64,
) -> bool {
	let Some(shortroomid) = self.shortroomid(room_id).await else {
		return false;
	};

	let candidate: Row<'_> = (shortroomid, event_id, Kind::Candidate.into());
	let marker: Row<'_> = (shortroomid, event_id, Kind::Redacted.into());

	self.db
		.roomid_softfailedsticky
		.qry(&candidate)
		.await
		.is_ok_and(|until| u64_from_bytes(&until).is_ok_and(|until| until > now))
		&& self
			.db
			.roomid_softfailedsticky
			.try_contains(&marker)
			.await
			.is_ok_and(|redacted| !redacted)
}

#[implement(Service)]
fn is_reevaluating(&self, event_id: &EventId) -> bool {
	self.reevaluating
		.lock()
		.expect("locked")
		.contains(event_id)
}

/// Queues a room whose installed state changed for the worker.
///
/// Nothing is weighed inline, since the caller holds the room's locks.
#[implement(Service)]
pub(crate) fn state_changed(&self, room_id: &RoomId) {
	let (sender, _) = &self.queue;

	// Closed only at shutdown, and the next start recovers a lost signal.
	sender.send(room_id.to_owned()).ok();
}

/// Weighs withheld sticky events again as rooms' installed state changes.
///
/// It first takes every room holding a candidate, so a signal lost to a
/// restart is recovered, then each queued room, coalescing a burst of changes
/// into one pass per room.
#[implement(Service)]
#[tracing::instrument(name = "withheld", level = "debug", skip_all)]
pub(super) async fn reevaluate_worker(&self) {
	let (_, receiver) = &self.queue;
	let rooms = self.candidate_rooms().await;

	self.reevaluate_rooms(rooms).await;

	while let Ok(room_id) = receiver.recv_async().await {
		let rooms = sorted_rooms(once(room_id).chain(receiver.drain()));

		self.reevaluate_rooms(rooms.into_iter().stream())
			.await;
	}
}

/// The rooms holding a candidate row.
///
/// Their short ids are read in full first, so no cursor stays open while the
/// rooms are weighed.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn candidate_rooms(&self) -> impl Stream<Item = OwnedRoomId> + Send + '_ {
	let shortroomids: Vec<ShortRoomId> = self
		.db
		.roomid_softfailedsticky
		.keys()
		.ignore_err()
		.ready_filter_map(|(shortroomid, _, kind): Row<'_>| {
			kind.eq(&u8::from(Kind::Candidate))
				.then_some(shortroomid)
		})
		.ready_scan(None, |last, shortroomid| Some(new_room(last, shortroomid)))
		.ready_filter_map(identity)
		.collect()
		.await;

	shortroomids
		.into_iter()
		.stream()
		.wide_filter_map(async |shortroomid| {
			self.services
				.short
				.get_roomid_from_short(shortroomid)
				.await
				.ok()
		})
}

#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn reevaluate_rooms<Rooms>(&self, rooms: Rooms)
where
	Rooms: Stream<Item = OwnedRoomId> + Send,
{
	rooms
		.for_each_concurrent(automatic_width(), async |room_id| {
			self.reevaluate_room(&room_id).await;
		})
		.await;
}

/// Runs a room's unexpired candidates through the incoming-event pipeline.
///
/// They are read in full first, so no cursor stays open across the pipeline's
/// fetches, then weighed one at a time, as each takes the room's federation
/// lock.
#[implement(Service)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		%room_id,
	),
)]
async fn reevaluate_room(&self, room_id: &RoomId) {
	let Some(shortroomid) = self.shortroomid(room_id).await else {
		return;
	};

	let now = now_millis();
	let candidates: Vec<OwnedEventId> = self
		.db
		.roomid_softfailedsticky
		.stream_prefix(&(shortroomid, Interfix))
		.ignore_err()
		.ready_filter(|((_, _, kind), until): &(Row<'_>, u64)| {
			*kind == u8::from(Kind::Candidate) && *until > now
		})
		.map(|((_, event_id, _), _)| event_id.to_owned())
		.collect()
		.await;

	// Every state change lands here, and nearly every room has no candidate.
	if candidates.is_empty() {
		return;
	}

	let room_version = self
		.services
		.state
		.get_room_version(room_id)
		.await
		.inspect_err(
			|error| warn!(%room_id, %error, "Unreadable room version for withheld events"),
		);

	let Ok(room_version) = room_version else {
		return;
	};

	candidates
		.iter()
		.stream()
		.for_each(async |event_id| {
			self.reevaluate(room_id, &room_version, shortroomid, event_id)
				.await;
		})
		.await;
}

/// Weighs one withheld event again, bypassing only the backoff that paces its
/// redelivery.
///
/// The stored copy goes back to the form its origin delivered, which alone its
/// signatures cover. An event the pipeline accepts, or finds already accepted,
/// leaves the candidates, as does one whose stored copy is gone; one withheld
/// again stays for the next change.
#[implement(Service)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		%event_id,
	),
)]
async fn reevaluate(
	&self,
	room_id: &RoomId,
	room_version: &RoomVersionId,
	shortroomid: ShortRoomId,
	event_id: &EventId,
) {
	let event_handler = &self.services.event_handler;
	let candidate: Row<'_> = (shortroomid, event_id, Kind::Candidate.into());
	let pdu = match self
		.services
		.timeline
		.get_outlier_pdu_json(event_id)
		.await
	{
		| Ok(pdu) => with_origin(into_outgoing_federation(pdu, room_version)),
		| Err(error) if error.is_not_found() => None,
		| Err(error) => {
			// Kept for the next change, which may read it.
			warn!(%room_id, %event_id, %error, "Unreadable withheld sticky event");
			return;
		},
	};

	let Some((pdu, origin)) = pdu else {
		self.db.roomid_softfailedsticky.del(candidate);
		return;
	};

	let _federation = event_handler.mutex_federation.lock(room_id).await;

	event_handler
		.clear_delivery_backoff(event_id)
		.await;

	self.reevaluating
		.lock()
		.expect("locked")
		.insert(event_id.to_owned());

	let outcome = event_handler
		.handle_incoming_pdu(&origin, room_id, event_id, pdu, true)
		.await;

	self.reevaluating
		.lock()
		.expect("locked")
		.remove(event_id);

	match outcome {
		| Ok(Some(_)) => self.db.roomid_softfailedsticky.del(candidate),
		| Ok(None) => debug!(%room_id, %event_id, "Sticky event still withheld"),
		| Err(error) => {
			debug_warn!(%room_id, %event_id, %error, "Failed to weigh a withheld sticky event");
		},
	}
}

/// Pairs a stored event with its sender's server, which the pipeline takes as
/// the origin.
///
/// A stored event keeps no record of the transaction that brought it, so its
/// sender's server stands in; an event without a valid sender pairs with none.
fn with_origin(pdu: CanonicalJsonObject) -> Option<(CanonicalJsonObject, OwnedServerName)> {
	let origin = pdu
		.get("sender")
		.and_then(CanonicalJsonValue::as_str)
		.and_then(|sender| <&UserId>::try_from(sender).ok())
		.map(|sender| sender.server_name().to_owned())?;

	Some((pdu, origin))
}

fn sorted_rooms<Queued>(queued: Queued) -> Rooms
where
	Queued: IntoIterator<Item = OwnedRoomId>,
{
	let mut rooms: Rooms = queued.into_iter().collect();

	rooms.sort_unstable();
	rooms.dedup();
	rooms
}

/// The short id of a room not seen just before it in key order.
///
/// A room's rows are adjacent in the key stream, so comparing each id with the
/// last one seen yields every room once.
fn new_room(last: &mut Option<ShortRoomId>, shortroomid: ShortRoomId) -> Option<ShortRoomId> {
	last.replace(shortroomid)
		.ne(&Some(shortroomid))
		.then_some(shortroomid)
}
