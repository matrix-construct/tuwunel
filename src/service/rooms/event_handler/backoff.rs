use std::{ops::Range, time::Duration};

use ruma::EventId;
use tuwunel_core::{
	Error, implement,
	utils::{
		continue_exponential_backoff,
		stream::{ReadyExt, TryIgnore},
		time::now_secs,
	},
};
use tuwunel_database::{Ignore, Interfix};

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

#[implement(super::Service)]
pub(super) async fn is_suppressed(
	&self,
	ctx: Context,
	event_id: &EventId,
	range: Range<Duration>,
) -> Suppression {
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

	(rate_ok && continue_exponential_backoff(range.start, range.end, elapsed, tries))
		.then_some(Suppression::Deny)
		.unwrap_or(Suppression::Allow)
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
