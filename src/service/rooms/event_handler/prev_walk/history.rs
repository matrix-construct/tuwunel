use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{Stream, StreamExt};
use ruma::{EventId, OwnedEventId, OwnedRoomId, OwnedServerName, RoomId, ServerName};
use tuwunel_core::{
	implement,
	utils::{
		math::{u64_from_u128_saturating, u64_from_usize_saturating},
		stream::{ReadyExt, TryIgnore},
		time::{now_millis, timepoint_from_epoch},
	},
};
use tuwunel_database::Interfix;

use super::{Outcome, Pass, PrevUpgrade};

/// Key of a pass row.
///
/// In order: the room, the wall clock in milliseconds since the Unix epoch when
/// the pass ended, and the incoming event. Rows sort by room, then by end.
pub(super) type PassKey<'a> = (&'a RoomId, u64, &'a EventId);

/// Value of a pass row.
///
/// In order: the outcome code, the collected prevs, the unprocessed prevs,
/// whether the fetch was capped as 0 or 1, the fetch and upgrade milliseconds,
/// and the origin.
pub(super) type PassVal<'a> = (u8, u64, u64, u8, u64, u64, &'a str);

pub(super) type PassRow<'a> = (PassKey<'a>, PassVal<'a>);

/// Recorded prev walk totals for one room.
///
/// Every recorded pass counts in `passes`. A pass whose outcome this build
/// does not know, written by a newer one, counts there and in the other totals
/// but in none of the outcome counts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrevWalkRoom {
	/// The room the passes ran in.
	pub room_id: OwnedRoomId,

	/// Recorded passes, whatever their outcome.
	pub passes: u64,

	/// Passes that appended the incoming event.
	pub appended: u64,

	/// Passes that finished without appending the incoming event.
	pub not_appended: u64,

	/// Passes whose walk returned an error.
	pub failed: u64,

	/// Passes whose walk was dropped or interrupted.
	pub cancelled: u64,

	/// Passes whose backward fetch returned an error.
	pub fetch_failed: u64,

	/// Passes dropped or interrupted before their walk began.
	pub fetch_cancelled: u64,

	/// Passes whose backward fetch hit the `max_fetch_prev_events` cap.
	pub capped: u64,

	/// Previous events the passes collected for upgrade.
	pub prevs: u64,

	/// Collected previous events left without an upgrade by passes that were
	/// not cancelled.
	pub unprocessed: u64,

	/// Time the passes spent before their walks began.
	pub fetch: Duration,

	/// Time the passes spent walking.
	pub upgrade: Duration,
}

/// One recorded pass.
///
/// Decoded from one row of the room's history. The origin is absent when the
/// recorded one does not parse, and the outcome when a newer build wrote a code
/// this one does not know.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrevWalkPass {
	/// When the pass ended, by the wall clock.
	pub ended: SystemTime,

	/// The gapped incoming event.
	pub event_id: OwnedEventId,

	/// Server the incoming event arrived from, absent when unparsable.
	pub origin: Option<OwnedServerName>,

	/// How the pass ended, absent when written by a newer build.
	pub outcome: Option<Outcome>,

	/// Previous events the pass collected for upgrade.
	pub prevs: u64,

	/// Collected previous events the pass left without an upgrade, none when it
	/// was cancelled.
	pub unprocessed: u64,

	/// Whether the backward fetch hit the `max_fetch_prev_events` cap.
	pub capped: bool,

	/// Time before the walk began, or until the end for a pass that never
	/// walked.
	pub fetch: Duration,

	/// Time spent walking.
	pub upgrade: Duration,
}

impl From<Outcome> for u8 {
	#[inline]
	fn from(outcome: Outcome) -> Self {
		match outcome {
			| Outcome::Held => 0,
			| Outcome::Closed => 1,
			| Outcome::FetchFailed => 2,
			| Outcome::FetchCancelled => 3,
			| Outcome::Appended => 4,
			| Outcome::NotAppended => 5,
			| Outcome::Failed => 6,
			| Outcome::Cancelled => 7,
		}
	}
}

impl TryFrom<u8> for Outcome {
	type Error = u8;

	#[inline]
	fn try_from(code: u8) -> Result<Self, Self::Error> {
		match code {
			| 0 => Ok(Self::Held),
			| 1 => Ok(Self::Closed),
			| 2 => Ok(Self::FetchFailed),
			| 3 => Ok(Self::FetchCancelled),
			| 4 => Ok(Self::Appended),
			| 5 => Ok(Self::NotAppended),
			| 6 => Ok(Self::Failed),
			| 7 => Ok(Self::Cancelled),
			| unknown => Err(unknown),
		}
	}
}

/// Totals for a room with no recorded passes.
///
/// Every count and time is zero, the base the room's rows are counted onto.
#[implement(PrevWalkRoom)]
#[must_use]
pub fn empty(room_id: &RoomId) -> Self {
	Self {
		room_id: room_id.to_owned(),
		passes: 0,
		appended: 0,
		not_appended: 0,
		failed: 0,
		cancelled: 0,
		fetch_failed: 0,
		fetch_cancelled: 0,
		capped: 0,
		prevs: 0,
		unprocessed: 0,
		fetch: Duration::ZERO,
		upgrade: Duration::ZERO,
	}
}

/// Record a reported pass in its room's history.
///
/// The row is keyed by the wall clock at the write, and passes ending in the
/// same millisecond stay apart by event. The write is synchronous and panics on
/// a read-only database, like the backoff attempt recorded on the same path.
#[implement(super::super::Service)]
// keeps the key and value buffers out of the guard's drop frame
#[inline(never)]
pub(super) fn record_pass(&self, upgrade: &PrevUpgrade<'_>, pass: &Pass) {
	let (key, val) = pass_row(upgrade, pass, now_millis());

	self.db.roomtseventid_prevwalk.put(key, val);
}

/// Encode a pass as the row recording it.
///
/// The end is given in milliseconds since the Unix epoch: the wall clock when
/// recording, a fixed time in a test.
pub(super) fn pass_row<'a>(upgrade: &PrevUpgrade<'a>, pass: &Pass, ended_ms: u64) -> PassRow<'a> {
	let PrevUpgrade { origin, room_id, event_id, .. } = *upgrade;
	let val = (
		u8::from(pass.outcome),
		u64_from_usize_saturating(pass.prevs),
		u64_from_usize_saturating(pass.unprocessed),
		u8::from(pass.capped),
		u64_from_u128_saturating(pass.fetch.as_millis()),
		u64_from_u128_saturating(pass.upgrade.as_millis()),
		origin.as_str(),
	);

	((room_id, ended_ms, event_id), val)
}

/// Read the recorded prev walk totals of every room, grouped by room in key
/// order.
///
/// The whole history is swept once, so the cost grows with the passes it keeps,
/// at most three days of them and fewer under load. A row that fails to decode
/// is skipped, or fails an assertion in a debug build.
#[implement(super::super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub async fn prev_walk_rooms(&self) -> impl ExactSizeIterator<Item = PrevWalkRoom> + Send {
	self.db
		.roomtseventid_prevwalk
		.stream()
		.ignore_err()
		.ready_fold(Vec::new(), tally)
		.await
		.into_iter()
}

/// Fold one row into the rooms read so far.
///
/// Rows arrive grouped by room, so a row either extends the last room or
/// starts the next.
pub(super) fn tally(
	mut rooms: Vec<PrevWalkRoom>,
	((room_id, ..), pass): PassRow<'_>,
) -> Vec<PrevWalkRoom> {
	match rooms.last_mut() {
		| Some(room) if room.room_id == room_id => room.count(pass),
		| _ => rooms.push(PrevWalkRoom::new(room_id, pass)),
	}

	rooms
}

#[implement(PrevWalkRoom)]
fn new(room_id: &RoomId, pass: PassVal<'_>) -> Self {
	let mut room = Self::empty(room_id);

	room.count(pass);
	room
}

#[implement(PrevWalkRoom)]
fn count(&mut self, (code, prevs, unprocessed, capped, fetch_ms, upgrade_ms, _): PassVal<'_>) {
	let counter = Outcome::try_from(code)
		.ok()
		.and_then(|outcome| match outcome {
			| Outcome::Held | Outcome::Closed => None,
			| Outcome::Appended => Some(&mut self.appended),
			| Outcome::NotAppended => Some(&mut self.not_appended),
			| Outcome::Failed => Some(&mut self.failed),
			| Outcome::Cancelled => Some(&mut self.cancelled),
			| Outcome::FetchFailed => Some(&mut self.fetch_failed),
			| Outcome::FetchCancelled => Some(&mut self.fetch_cancelled),
		});

	if let Some(counter) = counter {
		*counter = counter.saturating_add(1);
	}

	self.passes = self.passes.saturating_add(1);
	self.capped = self.capped.saturating_add(u64::from(capped != 0));
	self.prevs = self.prevs.saturating_add(prevs);
	self.unprocessed = self.unprocessed.saturating_add(unprocessed);
	self.fetch = self
		.fetch
		.saturating_add(Duration::from_millis(fetch_ms));

	self.upgrade = self
		.upgrade
		.saturating_add(Duration::from_millis(upgrade_ms));
}

/// Stream the recorded passes of one room, latest first.
///
/// A row that fails to decode is skipped, or fails an assertion in a debug
/// build.
#[implement(super::super::Service)]
pub fn prev_walk_passes<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = PrevWalkPass> + Send + 'a {
	let rows = self
		.db
		.roomtseventid_prevwalk
		.rev_stream_from(&(room_id, u64::MAX, Interfix))
		.ignore_err();

	room_passes(rows, room_id)
}

/// Take the leading rows of one room from a stream of rows, as passes.
///
/// The stream ends at the first row of another room without reading past it.
pub(super) fn room_passes<'a, S>(
	rows: S,
	room_id: &'a RoomId,
) -> impl Stream<Item = PrevWalkPass> + Send + 'a
where
	S: Stream<Item = PassRow<'a>> + Send + 'a,
{
	rows.ready_take_while(move |((room, ..), _)| *room == room_id)
		.map(PrevWalkPass::from_row)
}

#[implement(PrevWalkPass)]
fn from_row(((_, ended_ms, event_id), val): PassRow<'_>) -> Self {
	let (code, prevs, unprocessed, capped, fetch_ms, upgrade_ms, origin) = val;

	Self {
		ended: timepoint_from_epoch(Duration::from_millis(ended_ms)).unwrap_or(UNIX_EPOCH),
		event_id: event_id.to_owned(),
		origin: ServerName::parse(origin).ok(),
		outcome: Outcome::try_from(code).ok(),
		prevs,
		unprocessed,
		capped: capped != 0,
		fetch: Duration::from_millis(fetch_ms),
		upgrade: Duration::from_millis(upgrade_ms),
	}
}
