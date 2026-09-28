use std::{
	sync::atomic::{AtomicU64, Ordering},
	time::Instant,
};

use tuwunel_core::{Error, Result, implement, info, utils::math::fetch_add_usize, warn};

use super::{fetch_prev::PrevFetch, handle_prev_pdu::PrevUpgrade};

/// Snapshot of process-lifetime totals for incoming previous-event walks.
///
/// Each counter is loaded independently, so totals within one snapshot need
/// not reconcile while passes are in flight.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PrevWalkMetrics {
	/// Top-level incoming timeline events that reached the gap check.
	pub entered: u64,

	/// Events with a previous event missing from the timeline.
	pub gapped: u64,

	/// Gapped events withheld by a backoff hold.
	pub held: u64,

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
	walked: AtomicU64,
	walked_prevs: AtomicU64,
	capped: AtomicU64,
	appended: AtomicU64,
	not_appended: AtomicU64,
	failed: AtomicU64,
	cancelled: AtomicU64,
	unprocessed_prevs: AtomicU64,
}

/// Drop guard over one pass upgrading an incoming event's previous events.
///
/// Dropping the guard records the pass and logs one line with the outcome given
/// to `settle`, or as cancelled when dropped unsettled or interrupted. The
/// logged elapsed time covers the upgrade, after the backward fetch.
#[clippy::has_significant_drop]
#[must_use]
pub(super) struct PrevWalk<'a> {
	counters: &'a PrevWalkCounters,
	upgrade: PrevUpgrade<'a>,
	started: Instant,
	prevs: usize,
	capped: bool,
	settled: Option<(Outcome, usize)>,
}

#[derive(Clone, Copy)]
enum Outcome {
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

#[implement(PrevWalkCounters)]
pub(super) fn hold(&self) { self.held.fetch_add(1, Ordering::Relaxed); }

#[implement(PrevWalk, generics = "<'a>", params = "<'a>")]
pub(super) fn start(
	counters: &'a PrevWalkCounters,
	upgrade: PrevUpgrade<'a>,
	fetch: &PrevFetch,
) -> Self {
	let prevs = fetch.pdus.len();

	counters.start_walk(prevs);

	Self {
		counters,
		upgrade,
		started: Instant::now(),
		prevs,
		capped: fetch.capped,
		settled: None,
	}
}

#[implement(PrevWalkCounters)]
fn start_walk(&self, prevs: usize) {
	self.walked.fetch_add(1, Ordering::Relaxed);
	fetch_add_usize(&self.walked_prevs, prevs, Ordering::Relaxed);
}

#[implement(PrevWalk, params = "<'_>")]
pub(super) fn settle(mut self, appended: Result<bool, &Error>, upgraded: usize, stopping: bool) {
	debug_assert!(upgraded <= self.prevs, "upgraded more previous events than were collected");

	let outcome = Outcome::new(appended, stopping);
	let unprocessed = match outcome {
		| Outcome::Cancelled => 0,
		| _ => self.prevs.saturating_sub(upgraded),
	};

	self.settled = Some((outcome, unprocessed));
}

impl Drop for PrevWalk<'_> {
	fn drop(&mut self) {
		let Self {
			counters,
			upgrade,
			started,
			prevs,
			capped,
			settled,
		} = *self;

		let PrevUpgrade { origin, room_id, event_id, .. } = upgrade;
		let (outcome, unprocessed) = settled.unwrap_or((Outcome::Cancelled, 0));
		let elapsed_ms = started.elapsed().as_millis();

		counters.settle_walk(outcome, capped, unprocessed);

		if capped || matches!(outcome, Outcome::Failed | Outcome::Cancelled) {
			warn!(
				%room_id,
				%event_id,
				%origin,
				outcome = outcome.name(),
				prevs,
				unprocessed,
				capped,
				elapsed_ms,
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
				elapsed_ms,
				"Prev walk ended."
			);
		}
	}
}

#[implement(PrevWalkCounters)]
fn settle_walk(&self, outcome: Outcome, capped: bool, unprocessed: usize) {
	let counter = match outcome {
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

impl Outcome {
	/// An error while the server stops, or an interruption, is a cut-off rather
	/// than a verdict on the pass.
	fn new(appended: Result<bool, &Error>, stopping: bool) -> Self {
		match appended {
			| Ok(true) => Self::Appended,
			| Ok(false) => Self::NotAppended,
			| Err(error) if stopping || error.is_interrupted() => Self::Cancelled,
			| Err(_) => Self::Failed,
		}
	}

	fn name(self) -> &'static str {
		match self {
			| Self::Appended => "appended",
			| Self::NotAppended => "not_appended",
			| Self::Failed => "failed",
			| Self::Cancelled => "cancelled",
		}
	}
}
