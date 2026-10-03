use std::pin::pin;

use futures::{StreamExt, future::OptionFuture};
use ruma::ServerName;
use tuwunel_core::{
	Error, debug_info, extract_variant, implement,
	smallvec::SmallVec,
	utils::{IterStream, ReadyExt},
	warn,
};
use tuwunel_matrix::ShortRoomId;

use super::PDU_LIMIT;
use crate::{
	federation::is_content_rejection,
	sending::{
		Destination, SendingEvent, Service,
		data::{Park, QueueItem},
	},
};

/// Rooms of a rejected transaction still to send.
///
/// A rejected transaction rarely spans more than a couple of rooms.
type Rooms = SmallVec<[ShortRoomId; 2]>;

pub(super) type Slice = (Vec<QueueItem>, Split);

/// Consecutive failures of a transaction before its rooms are sent apart.
///
/// The first few retries ride out a transient fault before the batch is blamed.
const SPLIT_AFTER: u32 = 4;

/// The rooms of a transaction a destination kept rejecting, sent one at a time.
///
/// The head room is the one in flight. Until a room is delivered, each failure
/// moves the head last behind one more queued room brought in as a control. A
/// room that fails after a delivery is parked. Boxed to keep transaction
/// statuses small.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct Split(Box<Rounds>);

#[derive(Debug, Eq, PartialEq)]
struct Rounds {
	rooms: Rooms,
	delivered: bool,
}

/// Choose how a failed federation transaction continues.
///
/// After repeated failures the transaction's PDU rows return to the queue and
/// its rooms go out one per transaction; a room failing after a delivery is
/// parked. Returns the split to keep and the failure count its retry backs off
/// by, zero when the split advanced.
#[implement(Service)]
pub(super) async fn split_failure(
	&self,
	server: &ServerName,
	error: &Error,
	split: Option<Split>,
	tries: u32,
) -> (Option<Split>, u32) {
	let implicated = implicates_content(error);

	if split.is_none() && (!implicated || tries < SPLIT_AFTER) {
		return (None, tries);
	}

	let dest = Destination::Federation(server.to_owned());
	let active: Vec<_> = self.db.active_requests_for(&dest).collect().await;
	let rejected = split
		.as_ref()
		.filter(|split| implicated && split.0.delivered)
		.and_then(Split::head)
		.map(|room| self.db.next_park(server, room));

	let park = OptionFuture::from(rejected).await;

	self.db
		.demote(&active, park.map(|park| (server, park)));

	if let Some(Park { room, until, count }) = park {
		warn!(%server, room, until, count, "Parked a room the server keeps rejecting");
	}

	match split {
		| Some(split) if !implicated => (Some(split), tries),
		| Some(split) if split.0.delivered => (Some(split.skipped()), 0),
		| Some(split) => {
			let control = self.control(&dest, server, &split.0.rooms).await;

			(Some(split.rotated(control)), tries)
		},
		| None => start(server, &active, tries),
	}
}

/// Find the first queued room outside the split and the server's parks.
///
/// A control delivered while the split's rooms keep failing implicates those
/// rooms rather than the destination.
#[implement(Service)]
async fn control(
	&self,
	dest: &Destination,
	server: &ServerName,
	rooms: &Rooms,
) -> Option<ShortRoomId> {
	let skip: Rooms = self
		.db
		.parks(server)
		.map(|park| park.room)
		.chain(rooms.iter().copied().stream())
		.collect()
		.await;

	let skip = sorted(skip);

	pin!(self.db.queued_except(dest, &skip))
		.ready_find_map(|item| pdu_room(&item))
		.await
}

/// Advance a split past its delivered head room, ending any park it had.
///
/// Returns the next room's transaction, or nothing once the split is done.
#[implement(Service)]
pub(super) async fn split_delivered(&self, dest: &Destination, split: Split) -> Option<Slice> {
	if let (Destination::Federation(server), Some(room)) = (dest, split.head()) {
		self.db.unpark(server, room);
	}

	let next = self.slice(dest, split.delivered()).await;

	if next.is_none() {
		debug_info!(?dest, "Finished sending rooms apart");
	}

	next
}

/// Promote the head room's queued rows as the split's next transaction.
///
/// The split ends when it has no rooms left, or its head room has no rows. The
/// rejected transaction's EDU rows stay active and ride along until delivered.
#[implement(Service)]
pub(super) async fn slice(&self, dest: &Destination, split: Split) -> Option<Slice> {
	let room = split.head()?;
	let rows: Vec<_> = self
		.db
		.queued_room(dest, room)
		.take(PDU_LIMIT)
		.collect()
		.await;

	if rows.is_empty() {
		return None;
	}

	self.db.mark_as_active(rows.iter());

	let items = self.db.active_requests_for(dest).collect().await;

	Some((items, split))
}

#[implement(Split)]
fn new(rooms: Rooms) -> Self { Self(Box::new(Rounds { rooms, delivered: false })) }

/// Retry a room whose park expired.
///
/// The probe counts as delivered, so a failure parks the room again at once.
#[implement(Split)]
pub(super) fn probe(room: ShortRoomId) -> Self {
	Self(Box::new(Rounds {
		rooms: Rooms::from_slice(&[room]),
		delivered: true,
	}))
}

#[implement(Split)]
#[inline]
fn head(&self) -> Option<ShortRoomId> { self.0.rooms.first().copied() }

#[implement(Split)]
fn delivered(mut self) -> Self {
	self.0.delivered = true;
	self.skipped()
}

#[implement(Split)]
fn skipped(mut self) -> Self {
	self.0.rooms.drain(..self.0.rooms.len().min(1));
	self
}

#[implement(Split)]
fn rotated(mut self, control: Option<ShortRoomId>) -> Self {
	self.0.rooms.rotate_left(1);
	self.0.rooms.insert_many(0, control);
	self
}

/// Whether a failure answers for the transaction's content, not the path to the peer.
///
/// Content rejections and server errors come from the peer, and a timeout after
/// connecting may be the peer stalling on the content. Connection failures and
/// rate limits say nothing about it.
fn implicates_content(error: &Error) -> bool {
	match error {
		| Error::Federation(_, response) =>
			response.status_code.is_server_error() || is_content_rejection(error),
		| Error::Reqwest(error) => error.is_timeout() && !error.is_connect(),
		| _ => false,
	}
}

fn start(server: &ServerName, active: &[QueueItem], tries: u32) -> (Option<Split>, u32) {
	let rooms = sorted(active.iter().filter_map(pdu_room).collect());

	if rooms.is_empty() {
		return (None, tries);
	}

	debug_info!(%server, ?rooms, "Sending a rejected transaction's rooms apart");
	(Some(Split::new(rooms)), 0)
}

fn sorted(mut rooms: Rooms) -> Rooms {
	rooms.sort_unstable();
	rooms.dedup();
	rooms
}

pub(super) fn pdu_room((_, event): &QueueItem) -> Option<ShortRoomId> {
	extract_variant!(event, SendingEvent::Pdu).map(|id| u64::from_be_bytes(id.shortroomid()))
}

#[cfg(test)]
mod tests;
