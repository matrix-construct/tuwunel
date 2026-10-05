use std::{
	ops::Range,
	sync::atomic::{AtomicU64, Ordering},
	time::Duration,
};

use futures::FutureExt;
use ruma::EventId;
use tuwunel_core::{
	Error, implement,
	utils::{
		BoolExt, continue_exponential_backoff,
		stream::{ReadyExt, TryIgnore},
		time::now_secs,
	},
};
use tuwunel_database::{Ignore, Interfix};

#[cfg(test)]
mod tests;

/// Bucket width in seconds. Records within one bucket collide onto a single key
/// (`<=` the smallest call-site backoff floor), coalescing concurrent failures.
const QUANTUM: u64 = 60;

/// Accumulated `Pending` records at which the rate brake engages.
const SUPPRESS_AFTER: u32 = 3;

/// Retry window for the `Upgrade` context.
///
/// Bounds how often a re-delivered event repeats the full upgrade. A
/// soft-failed event is re-evaluated on this widening schedule rather than
/// rejected forever, so a lapsed policy-server refusal heals.
pub(super) const UPGRADE_RETRY: Range<Duration> =
	Duration::from_mins(5)..Duration::from_hours(24);

/// Federation step that recorded a decision; the key's leading discriminant.
#[derive(Clone, Copy)]
pub(super) enum Context {
	Fetch = 0,
	Auth = 1,
	Upgrade = 2,
	Incoming = 3,
}

impl From<Context> for u8 {
	#[inline]
	fn from(context: Context) -> Self {
		match context {
			| Context::Fetch => 0,
			| Context::Auth => 1,
			| Context::Upgrade => 2,
			| Context::Incoming => 3,
		}
	}
}

/// Permanence of a recorded decision. Unknown discriminants decode to the
/// weakest (`Pending`) so a future encoding can only soften, never wrongly
/// escalate, a verdict against an old binary. `Permanent` is never written by
/// this store.
#[derive(Clone, Copy, Default)]
pub(super) enum Disposition {
	#[default]
	Pending = 0,
	Transient = 1,
	Permanent = 2,
}

/// Verdict from consulting the store before a federation step.
pub(super) enum Suppression {
	/// No row is recorded for the event in this context.
	Absent,

	/// Rows are recorded but no hold is in force.
	Allow,

	/// A hold is in force; skip the step.
	Deny,
}

#[derive(Default)]
struct Summary {
	total: u32,
	pending: u32,
	latest_secs: u64,
	latest_class: Disposition,
}

/// Snapshot of process-lifetime verdicts from the backoff store, per federation
/// step.
///
/// Every lookup that completes lands in exactly one verdict of its step; one
/// dropped before the store answers lands in none. Each counter is loaded
/// independently, so totals within one snapshot need not reconcile while
/// lookups are in flight.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BackoffMetrics {
	/// Lookups before an event or its auth chain is fetched over federation.
	pub fetch: Verdicts,

	/// Lookups before handling a fetched event from an auth chain.
	pub auth: Verdicts,

	/// Lookups before upgrading a previous event, or re-weighing a soft-failed
	/// one.
	pub upgrade: Verdicts,

	/// Lookups before walking a gapped incoming event's previous events.
	pub incoming: Verdicts,
}

/// Verdict counts for one federation step.
///
/// Each completed lookup of the step increments exactly one field, so
/// [`Verdicts::lookups`] is the step's lookup count.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Verdicts {
	/// Lookups that found no row for the event.
	pub absent: u64,

	/// Lookups that found rows but no hold in force.
	pub allowed: u64,

	/// Lookups that found a hold in force and skipped the step.
	pub denied: u64,
}

#[derive(Default)]
pub(super) struct BackoffCounters {
	fetch: VerdictCounters,
	auth: VerdictCounters,
	upgrade: VerdictCounters,
	incoming: VerdictCounters,
}

#[derive(Default)]
struct VerdictCounters {
	absent: AtomicU64,
	allowed: AtomicU64,
	denied: AtomicU64,
}

impl From<u64> for Disposition {
	#[inline]
	fn from(disc: u64) -> Self {
		match disc {
			| 1 => Self::Transient,
			| 2 => Self::Permanent,
			| _ => Self::Pending,
		}
	}
}

impl From<Disposition> for u64 {
	#[inline]
	fn from(disposition: Disposition) -> Self {
		match disposition {
			| Disposition::Pending => 0,
			| Disposition::Transient => 1,
			| Disposition::Permanent => 2,
		}
	}
}

impl Suppression {
	#[inline]
	pub(super) fn is_deny(&self) -> bool { matches!(self, Self::Deny) }
}

impl Summary {
	fn tally(mut self, (_, (class, secs)): (Ignore, (u64, u64))) -> Self {
		let class = Disposition::from(class);

		self.total = self.total.saturating_add(1);
		if matches!(class, Disposition::Pending) {
			self.pending = self.pending.saturating_add(1);
		}

		if secs >= self.latest_secs {
			self.latest_secs = secs;
			self.latest_class = class;
		}

		self
	}
}

impl Verdicts {
	/// Lookups of the step, whatever their verdict.
	///
	/// Saturates at `u64::MAX` rather than wrapping.
	#[inline]
	#[must_use]
	pub fn lookups(&self) -> u64 {
		self.absent
			.saturating_add(self.allowed)
			.saturating_add(self.denied)
	}
}

/// Read a snapshot of backoff verdict totals.
///
/// Reading does not reset the totals.
#[implement(super::Service)]
#[inline]
#[must_use]
pub fn backoff_metrics(&self) -> BackoffMetrics { self.backoff.snapshot() }

#[implement(BackoffCounters)]
fn snapshot(&self) -> BackoffMetrics {
	BackoffMetrics {
		fetch: self.fetch.snapshot(),
		auth: self.auth.snapshot(),
		upgrade: self.upgrade.snapshot(),
		incoming: self.incoming.snapshot(),
	}
}

impl VerdictCounters {
	fn snapshot(&self) -> Verdicts {
		Verdicts {
			absent: self.absent.load(Ordering::Relaxed),
			allowed: self.allowed.load(Ordering::Relaxed),
			denied: self.denied.load(Ordering::Relaxed),
		}
	}
}

/// Record a federation attempt before a cancellable await, so a premature
/// cancellation still leaves a `Pending` row behind to rate-gate against.
///
/// Returns the attempt's bucket for `record_completion` to settle.
#[implement(super::Service)]
pub(super) fn record_attempt(&self, ctx: Context, event_id: &EventId) -> u32 {
	let bucket = current_bucket();

	self.record_outcome_at(ctx, event_id, bucket, Disposition::Pending);
	bucket
}

/// Settle a step once its work has finished.
///
/// An appended event clears what the context holds for it: only the attempt's
/// own row when nothing preceded it, every row otherwise. A failure is recorded
/// as `Transient` in the bucket of the attempt it settles, replacing that
/// attempt's `Pending` row, or in the current bucket when rows preceded the
/// step but no attempt was written. A withheld event, a failure from an
/// interruption or shutdown, or a step with no rows and no attempt records
/// nothing. Rows an earlier gapped pass left behind outlive a later append
/// that found no gap, until the store reaps them.
#[implement(super::Service)]
pub(super) async fn record_completion(
	&self,
	ctx: Context,
	event_id: &EventId,
	standing: Suppression,
	attempt: Option<u32>,
	appended: Result<bool, &Error>,
) {
	match (appended, standing, attempt) {
		| (_, Suppression::Absent, None) | (Ok(false), ..) => {},
		| (Err(error), ..) if error.is_interrupted() || self.services.server.is_stopping() => {},
		| (Ok(true), Suppression::Absent, Some(bucket)) =>
			self.clear_outcome_at(ctx, event_id, bucket),
		| (Ok(true), ..) => self.record_success(ctx, event_id).await,
		| (Err(_), _, attempt) => {
			let bucket = attempt.unwrap_or_else(current_bucket);

			self.record_outcome_at(ctx, event_id, bucket, Disposition::Transient);
		},
	}
}

#[implement(super::Service)]
pub(super) fn record_outcome(&self, ctx: Context, event_id: &EventId, disposition: Disposition) {
	if self.services.server.is_stopping() {
		return;
	}

	self.record_outcome_at(ctx, event_id, current_bucket(), disposition);
}

/// Clears the backoffs recorded against an event's delivery.
///
/// The soft-fail marker and these backoffs gate the same retry, so operator
/// recovery has to drop all of them before the event is next evaluated,
/// whether by a redelivery or as the prev of a later event.
#[implement(super::Service)]
pub async fn clear_delivery_backoff(&self, event_id: &EventId) {
	self.record_success(Context::Upgrade, event_id)
		.await;

	self.record_success(Context::Incoming, event_id)
		.await;
}

#[implement(super::Service)]
pub(super) async fn record_success(&self, ctx: Context, event_id: &EventId) {
	self.db
		.eventid_backoff
		.del_prefix(&(u8::from(ctx), event_id, Interfix))
		.await;
}

/// Consult the store before a federation step, counting the verdict.
///
/// The verdict is counted when the store answers, so a lookup dropped before
/// then counts nothing.
#[implement(super::Service)]
pub(super) fn is_suppressed(
	&self,
	ctx: Context,
	event_id: &EventId,
	range: Range<Duration>,
) -> impl Future<Output = Suppression> + Send {
	self.verdict(ctx, event_id, range)
		.inspect(move |verdict| self.backoff.count(ctx, verdict))
}

#[implement(super::Service)]
async fn verdict(&self, ctx: Context, event_id: &EventId, range: Range<Duration>) -> Suppression {
	let summary = self
		.db
		.eventid_backoff
		.stream_prefix::<Ignore, (u64, u64), _>(&(u8::from(ctx), event_id, Interfix))
		.ignore_err()
		.ready_fold(Summary::default(), Summary::tally)
		.await;

	if summary.total == 0 {
		return Suppression::Absent;
	}

	if matches!(summary.latest_class, Disposition::Permanent) {
		return Suppression::Deny;
	}

	let elapsed = Duration::from_secs(now_secs().saturating_sub(summary.latest_secs));
	let (tries, rate_ok) = match summary.latest_class {
		| Disposition::Pending => (summary.pending, summary.pending >= SUPPRESS_AFTER),
		| _ => (summary.total, true),
	};

	let deny = rate_ok && continue_exponential_backoff(range.start, range.end, elapsed, tries);

	deny.map_or(Suppression::Allow, || Suppression::Deny)
}

#[implement(BackoffCounters)]
fn count(&self, ctx: Context, verdict: &Suppression) {
	let counters = match ctx {
		| Context::Fetch => &self.fetch,
		| Context::Auth => &self.auth,
		| Context::Upgrade => &self.upgrade,
		| Context::Incoming => &self.incoming,
	};

	let counter = match verdict {
		| Suppression::Absent => &counters.absent,
		| Suppression::Allow => &counters.allowed,
		| Suppression::Deny => &counters.denied,
	};

	counter.fetch_add(1, Ordering::Relaxed);
}

fn current_bucket() -> u32 { u32::try_from(now_secs() / QUANTUM).unwrap_or(u32::MAX) }

#[implement(super::Service)]
fn record_outcome_at(
	&self,
	ctx: Context,
	event_id: &EventId,
	bucket: u32,
	disposition: Disposition,
) {
	self.db
		.eventid_backoff
		.put((u8::from(ctx), event_id, bucket), (u64::from(disposition), now_secs()));
}

#[implement(super::Service)]
fn clear_outcome_at(&self, ctx: Context, event_id: &EventId, bucket: u32) {
	self.db
		.eventid_backoff
		.del((u8::from(ctx), event_id, bucket));
}
