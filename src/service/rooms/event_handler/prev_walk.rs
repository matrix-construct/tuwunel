use std::{
	sync::atomic::{AtomicU64, Ordering},
	time::Instant,
};

use tuwunel_core::{Error, Result, implement, info, utils::math::fetch_add_usize, warn};

use super::{fetch_prev::PrevFetch, handle_prev_pdu::PrevUpgrade};

/// Snapshot of process-lifetime totals for incoming previous-event walks.
///
/// Every gapped event ends as exactly one of `held`, `closed`, `fetch_failed`,
/// `fetch_cancelled` or `walked`, and every walk as exactly one of `appended`,
/// `not_appended`, `failed` or `cancelled`. Each counter is loaded
/// independently, so totals within one snapshot need not reconcile while
/// passes are in flight.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PrevWalkMetrics {
	/// Top-level incoming timeline events that reached the gap check, whether from
	/// a federation transaction, a join, an invite or backfill.
	pub entered: u64,

	/// Events with a previous event missing from the timeline.
	pub gapped: u64,

	/// Gapped events withheld by a backoff hold.
	pub held: u64,

	/// Gapped events whose previous events all reached the timeline during the
	/// backward fetch, leaving nothing to walk.
	pub closed: u64,

	/// Gapped events whose backward fetch returned an error.
	pub fetch_failed: u64,

	/// Gapped events dropped before their walk began, or whose backward fetch
	/// was interrupted.
	pub fetch_cancelled: u64,

	/// Passes that walked previous events missing from the timeline.
	pub walked: u64,

	/// Previous events collected for upgrade by walking passes.
	pub walked_prevs: u64,

	/// Passes whose walk hit the `max_fetch_prev_events` cap.
	pub capped: u64,

	/// Passes that appended the incoming event.
	pub appended: u64,

	/// Passes that finished without appending the incoming event.
	pub not_appended: u64,

	/// Passes that returned an error.
	pub failed: u64,

	/// Passes dropped before they settled or interrupted by shutdown.
	pub cancelled: u64,

	/// Collected previous events left without an upgrade by passes that were
	/// not cancelled.
	pub unprocessed_prevs: u64,
}

#[derive(Default)]
pub(super) struct PrevWalkCounters {
	entered: AtomicU64,
	gapped: AtomicU64,
	held: AtomicU64,
	closed: AtomicU64,
	fetch_failed: AtomicU64,
	fetch_cancelled: AtomicU64,
	walked: AtomicU64,
	walked_prevs: AtomicU64,
	capped: AtomicU64,
	appended: AtomicU64,
	not_appended: AtomicU64,
	failed: AtomicU64,
	cancelled: AtomicU64,
	unprocessed_prevs: AtomicU64,
}

/// Drop guard over one gapped incoming event, from its backoff consult through
/// the backward fetch and the upgrade of its previous events.
///
/// Dropping the guard records the event under its outcome, or as a cancelled
/// fetch or walk when dropped unsettled. A walk, a failed fetch and a cancelled
/// fetch each log one line; a hold and a fetch that leaves nothing to walk log
/// none.
#[clippy::has_significant_drop]
#[must_use]
pub(super) struct PrevWalk<'a> {
	counters: &'a PrevWalkCounters,
	upgrade: &'a PrevUpgrade<'a>,
	started: Instant,
	walk: Option<Walk>,
	settled: Option<(Outcome, usize)>,
}

/// The walking phase, entered when the backward fetch returns previous events.
///
/// The guard holds none while the fetch runs, so an unsettled drop without one
/// counts as a cancelled fetch rather than a cancelled walk.
#[derive(Clone, Copy)]
struct Walk {
	fetched: Instant,
	prevs: usize,
	capped: bool,
}

#[derive(Clone, Copy)]
enum Outcome {
	Held,
	Closed,
	FetchFailed,
	FetchCancelled,
	Appended,
	NotAppended,
	Failed,
	Cancelled,
}

/// Read a snapshot of incoming previous-event walk totals.
///
/// Reading does not reset the totals.
#[implement(super::Service)]
#[inline]
#[must_use]
pub fn prev_walk_metrics(&self) -> PrevWalkMetrics { self.prev_walk.snapshot() }

#[implement(PrevWalkCounters)]
fn snapshot(&self) -> PrevWalkMetrics {
	PrevWalkMetrics {
		entered: self.entered.load(Ordering::Relaxed),
		gapped: self.gapped.load(Ordering::Relaxed),
		held: self.held.load(Ordering::Relaxed),
		closed: self.closed.load(Ordering::Relaxed),
		fetch_failed: self.fetch_failed.load(Ordering::Relaxed),
		fetch_cancelled: self.fetch_cancelled.load(Ordering::Relaxed),
		walked: self.walked.load(Ordering::Relaxed),
		walked_prevs: self.walked_prevs.load(Ordering::Relaxed),
		capped: self.capped.load(Ordering::Relaxed),
		appended: self.appended.load(Ordering::Relaxed),
		not_appended: self.not_appended.load(Ordering::Relaxed),
		failed: self.failed.load(Ordering::Relaxed),
		cancelled: self.cancelled.load(Ordering::Relaxed),
		unprocessed_prevs: self.unprocessed_prevs.load(Ordering::Relaxed),
	}
}

#[implement(PrevWalkCounters)]
pub(super) fn enter(&self, gapped: bool) {
	self.entered.fetch_add(1, Ordering::Relaxed);

	if gapped {
		self.gapped.fetch_add(1, Ordering::Relaxed);
	}
}

#[implement(PrevWalk, generics = "<'a>", params = "<'a>")]
pub(super) fn start(counters: &'a PrevWalkCounters, upgrade: &'a PrevUpgrade<'a>) -> Self {
	Self {
		counters,
		upgrade,
		started: Instant::now(),
		walk: None,
		settled: None,
	}
}

#[implement(PrevWalk, params = "<'_>")]
pub(super) fn hold(self) { self.end(Outcome::Held, 0); }

#[implement(PrevWalk, params = "<'_>")]
fn end(mut self, outcome: Outcome, unprocessed: usize) {
	self.settled = Some((outcome, unprocessed));
}

/// Record the end of the backward fetch.
///
/// Returns `None` once the pass is settled, when the fetch failed or left
/// nothing to walk. Otherwise hands the guard back for the walk, which `settle`
/// ends.
#[implement(PrevWalk, params = "<'_>")]
pub(super) fn fetched(self, fetch: Result<&PrevFetch, &Error>, stopping: bool) -> Option<Self> {
	let outcome = match fetch {
		| Err(error) => Outcome::fetch_error(error, stopping),
		| Ok(fetch) if fetch.sorted.is_empty() => Outcome::Closed,
		| Ok(fetch) => return Some(self.begin_walk(fetch)),
	};

	self.end(outcome, 0);

	None
}

#[implement(Outcome)]
fn fetch_error(error: &Error, stopping: bool) -> Self {
	if Self::cut_off(error, stopping) {
		Self::FetchCancelled
	} else {
		Self::FetchFailed
	}
}

/// Whether an error cut the pass off rather than failing it.
///
/// An error while the server stops, or an interruption, says nothing about the
/// events themselves.
#[implement(Outcome)]
fn cut_off(error: &Error, stopping: bool) -> bool { stopping || error.is_interrupted() }

#[implement(PrevWalk, params = "<'_>")]
fn begin_walk(mut self, fetch: &PrevFetch) -> Self {
	let prevs = fetch.pdus.len();
	let walk = Walk {
		fetched: Instant::now(),
		prevs,
		capped: fetch.capped,
	};

	self.counters.start_walk(prevs);
	self.walk = Some(walk);

	self
}

#[implement(PrevWalkCounters)]
fn start_walk(&self, prevs: usize) {
	self.walked.fetch_add(1, Ordering::Relaxed);
	fetch_add_usize(&self.walked_prevs, prevs, Ordering::Relaxed);
}

#[implement(PrevWalk, params = "<'_>")]
pub(super) fn settle(self, appended: Result<bool, &Error>, upgraded: usize, stopping: bool) {
	let prevs = self.walk.map_or(0, |walk| walk.prevs);

	debug_assert!(upgraded <= prevs, "upgraded more previous events than were collected");

	let outcome = Outcome::new(appended, stopping);
	let unprocessed = match outcome {
		| Outcome::Cancelled => 0,
		| _ => prevs.saturating_sub(upgraded),
	};

	self.end(outcome, unprocessed);
}

#[implement(Outcome)]
fn new(appended: Result<bool, &Error>, stopping: bool) -> Self {
	match appended {
		| Ok(true) => Self::Appended,
		| Ok(false) => Self::NotAppended,
		| Err(error) if Self::cut_off(error, stopping) => Self::Cancelled,
		| Err(_) => Self::Failed,
	}
}

impl Drop for PrevWalk<'_> {
	fn drop(&mut self) {
		let Self {
			counters,
			upgrade,
			started,
			walk,
			settled,
		} = *self;

		let unsettled = walk.map_or(Outcome::FetchCancelled, |_| Outcome::Cancelled);
		let (outcome, unprocessed) = settled.unwrap_or((unsettled, 0));
		let capped = walk.is_some_and(|walk| walk.capped);

		counters.settle_pass(outcome, capped, unprocessed);

		match walk {
			| None => log_fetch_end(upgrade, outcome, started),
			| Some(walk) => log_walk_end(upgrade, outcome, started, walk, unprocessed),
		}
	}
}

#[implement(PrevWalkCounters)]
fn settle_pass(&self, outcome: Outcome, capped: bool, unprocessed: usize) {
	let counter = match outcome {
		| Outcome::Held => &self.held,
		| Outcome::Closed => &self.closed,
		| Outcome::FetchFailed => &self.fetch_failed,
		| Outcome::FetchCancelled => &self.fetch_cancelled,
		| Outcome::Appended => &self.appended,
		| Outcome::NotAppended => &self.not_appended,
		| Outcome::Failed => &self.failed,
		| Outcome::Cancelled => &self.cancelled,
	};

	counter.fetch_add(1, Ordering::Relaxed);

	if capped {
		self.capped.fetch_add(1, Ordering::Relaxed);
	}

	fetch_add_usize(&self.unprocessed_prevs, unprocessed, Ordering::Relaxed);
}

fn log_fetch_end(upgrade: &PrevUpgrade<'_>, outcome: Outcome, started: Instant) {
	let PrevUpgrade { origin, room_id, event_id, .. } = *upgrade;

	if matches!(outcome, Outcome::FetchFailed | Outcome::FetchCancelled) {
		let fetch_ms = started.elapsed().as_millis();

		warn!(
			%room_id,
			%event_id,
			%origin,
			outcome = outcome.name(),
			fetch_ms,
			"Prev walk ended."
		);
	}
}

fn log_walk_end(
	upgrade: &PrevUpgrade<'_>,
	outcome: Outcome,
	started: Instant,
	Walk { fetched, prevs, capped }: Walk,
	unprocessed: usize,
) {
	let PrevUpgrade { origin, room_id, event_id, .. } = *upgrade;
	let fetch_ms = fetched.duration_since(started).as_millis();
	let upgrade_ms = fetched.elapsed().as_millis();

	if capped || matches!(outcome, Outcome::Failed | Outcome::Cancelled) {
		warn!(
			%room_id,
			%event_id,
			%origin,
			outcome = outcome.name(),
			prevs,
			unprocessed,
			capped,
			fetch_ms,
			upgrade_ms,
			"Prev walk ended."
		);
	} else {
		info!(
			%room_id,
			%event_id,
			%origin,
			outcome = outcome.name(),
			prevs,
			unprocessed,
			capped,
			fetch_ms,
			upgrade_ms,
			"Prev walk ended."
		);
	}
}

#[implement(Outcome)]
fn name(self) -> &'static str {
	match self {
		| Self::Held => "held",
		| Self::Closed => "closed",
		| Self::FetchFailed => "fetch_failed",
		| Self::FetchCancelled => "fetch_cancelled",
		| Self::Appended => "appended",
		| Self::NotAppended => "not_appended",
		| Self::Failed => "failed",
		| Self::Cancelled => "cancelled",
	}
}
