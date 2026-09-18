//! Stores and evaluates per-server federation reachability.
//!
//! Each failure is a blind write keyed by `(server, bucket)`, with repeated
//! failures in one window deliberately coalescing; current row values retain
//! the classification and exact failure time. A scan derives the latest
//! anchor and surviving bucket span before applying the retry curve. Successful
//! outbound or inbound contact clears the server prefix. The bucket width
//! comes from `sender_timeout`, keeping this gate aligned with sender backoff.

use std::{
	collections::BTreeMap,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures::{Stream, StreamExt};
use http::StatusCode;
use ruma::{OwnedServerName, ServerName, api::error::ErrorBody};
use tuwunel_core::{
	Error, implement,
	utils::{
		stream::{ReadyExt, TryIgnore},
		time::now_secs,
	},
};
use tuwunel_database::Interfix;

/// Caps peer-status backoff delays.
///
/// The duration matches the 24-hour default of `sender_retry_backoff_limit`.
pub(super) const MAX_BACKOFF: Duration = Duration::from_hours(24);

/// Permanence classification supplied alongside a failure.
///
/// The classification selects either the retry curve or the maximum backoff.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Classification {
	/// Marks a failure that may recover on a later attempt.
	#[default]
	Transient,

	/// Marks a peer as unavailable for the maximum backoff duration.
	Permanent,
}

impl Classification {
	/// Unknown bytes downgrade to `Transient`; a future encoding can only
	/// soften a verdict, never wrongly escalate one against an old binary.
	#[inline]
	#[must_use]
	fn from_byte(byte: u8) -> Self {
		match byte {
			| 1 => Self::Permanent,
			| _ => Self::Transient,
		}
	}
}

impl From<Classification> for u8 {
	#[inline]
	fn from(c: Classification) -> Self {
		match c {
			| Classification::Transient => 0,
			| Classification::Permanent => 1,
		}
	}
}

/// Verdict returned by [`super::Service::should_attempt`].
///
/// Callers can attempt immediately, defer until a deadline, or retain an
/// eligible peer behind preferred candidates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShouldAttempt {
	/// Allows the peer to be attempted immediately.
	Yes,

	/// Defers the peer until its current backoff expires.
	No {
		/// Earliest wall-clock time at which another attempt is allowed.
		earliest_retry: SystemTime,
	},

	/// Eligible but should be sorted to the back of any candidate list
	/// rather than skipped outright.
	Deprioritize,
}

/// Latest-failure state feeding the pure [`attempt_verdict`] decision.
///
/// Time values are injected as epoch seconds so the retry decision is
/// deterministic in tests.
pub(super) struct Backoff {
	/// Classification of the newest surviving failure.
	pub(super) class: Classification,

	/// Failure instant the delay is measured from (seconds since the epoch).
	pub(super) anchor_secs: u64,

	/// Number of failure windows represented by the surviving rows.
	pub(super) streak: u32,

	/// Current time (seconds since the epoch); injected for testability.
	pub(super) now: u64,

	/// Width of one coalescing window in seconds.
	pub(super) window_secs: u64,

	/// Initial delay applied to a lone transient failure.
	pub(super) grace_secs: u64,
}

/// Fold state accumulated over one server's failure rows.
///
/// Rows are folded in key order, retaining both ends of the surviving bucket
/// span while the newest row supplies the class and anchor.
#[derive(Clone, Copy)]
pub(super) struct Streak {
	/// Classification of the newest row.
	pub(super) class: Classification,

	/// Recorded failure instant of the newest row in epoch seconds.
	pub(super) anchor_secs: u64,

	/// Oldest bucket included in the streak.
	pub(super) oldest_bucket: u64,

	/// Newest bucket included in the streak.
	pub(super) latest_bucket: u64,
}

/// Summarizes a peer's current failure streak for administration.
///
/// The anchor and oldest values are Unix epoch seconds; `delay_secs` is the
/// retry duration applied from the anchor.
#[derive(Clone, Copy, Debug)]
pub struct PeerBackoff {
	/// Classification of the newest surviving failure.
	pub class: Classification,

	/// Newest failure instant, the backoff anchor.
	pub anchor_secs: u64,

	/// Start of the oldest surviving failure bucket.
	pub oldest_secs: u64,

	/// Backoff delay measured from the anchor.
	pub delay_secs: u64,
}

/// Clears all recorded failures after a successful outbound request.
///
/// Removing the peer prefix makes the next attempt immediately eligible.
#[implement(super::Service)]
pub async fn record_success(&self, server: &ServerName) {
	self.statuses
		.del_prefix(&(server, Interfix))
		.await;
}

/// Clears failure rows after a peer proves reachable through inbound activity.
///
/// The return value reports whether any rows were present, allowing the caller
/// to flush only after a change. A healthy-peer miss writes no tombstone.
#[implement(super::Service)]
#[tracing::instrument(
	level = "trace",
	skip(self),
	fields(
		%server,
	),
)]
pub async fn note_peer_alive(&self, server: &ServerName) -> bool {
	let sad = self.peer_has_failures(server).await;

	if sad {
		self.statuses
			.del_prefix(&(server, Interfix))
			.await;
	}

	sad
}

/// Reports whether the reachability store holds a failure row for a peer.
///
/// Database errors are ignored and therefore behave like an empty prefix.
#[implement(super::Service)]
#[tracing::instrument(
	level = "trace",
	skip(self),
	fields(
		%server,
	),
)]
pub async fn peer_has_failures(&self, server: &ServerName) -> bool {
	self.statuses
		.stream_prefix_raw(&(server, Interfix))
		.ignore_err()
		.ready_any(|_| true)
		.await
}

/// Records one classified failure in the peer's current time bucket.
///
/// Repeated failures in the same bucket overwrite the same row. The value also
/// stores the exact failure instant while remaining compatible with old rows.
#[implement(super::Service)]
pub fn record_failure(&self, server: &ServerName, classification: Classification) {
	// Raw-value additive extension; old one-byte rows stay readable.
	let mut value = [0_u8; 9];
	value[0] = u8::from(classification);
	value[1..].copy_from_slice(&now_secs().to_be_bytes());

	self.statuses
		.put_raw((server, self.current_bucket()), value);
}

/// Determines whether a federation request should currently target a peer.
///
/// A peer without readable failure rows is immediately eligible. Otherwise the
/// verdict is derived from its newest failure and surviving bucket span.
#[implement(super::Service)]
#[tracing::instrument(skip(self), fields(%server), level = "trace")]
pub async fn should_attempt(&self, server: &ServerName) -> ShouldAttempt {
	let Some(streak) = self.peer_streak(server).await else {
		return ShouldAttempt::Yes;
	};

	attempt_verdict(&self.backoff(streak))
}

/// Returns the current admin-facing backoff summary for one server.
///
/// The result is `None` when the peer has no readable failure rows.
#[implement(super::Service)]
pub async fn peer_backoff(&self, server: &ServerName) -> Option<PeerBackoff> {
	self.peer_streak(server)
		.await
		.map(|streak| self.peer_backoff_from(streak))
}

/// Returns admin-facing backoff summaries for all peers with failure rows.
///
/// The store is scanned once. Rows group by server on disk, so each contiguous
/// run of buckets is folded in place; database errors are skipped.
#[implement(super::Service)]
pub async fn peer_backoffs(&self) -> BTreeMap<OwnedServerName, PeerBackoff> {
	let window_secs = self.window_secs;

	self.statuses
		.stream()
		.ignore_err()
		.ready_fold(
			Vec::<(OwnedServerName, Streak)>::new(),
			|mut runs, ((server, bucket), value): ((&ServerName, u64), &[u8])| {
				match runs.last_mut() {
					| Some((last, streak)) if *last == *server =>
						*streak = fold_streak(window_secs, Some(*streak), bucket, value),
					| _ => runs
						.push((server.to_owned(), fold_streak(window_secs, None, bucket, value))),
				}

				runs
			},
		)
		.await
		.into_iter()
		.map(|(server, streak)| (server, self.peer_backoff_from(streak)))
		.collect()
}

/// Streams one tuple per readable peer-status bucket.
///
/// Items are ordered by `(server, bucket_start)` for the admin snapshot table.
/// Borrowed server names are cursor-backed and remain valid only until the next
/// poll. Database and key-decoding errors are skipped; malformed values decode
/// as transient failures.
#[implement(super::Service)]
pub fn peer_snapshot(
	&self,
) -> impl Stream<Item = (&ServerName, SystemTime, Classification)> + Send + '_ {
	self.statuses.stream().ignore_err().map(
		move |((server, bucket), value): ((&ServerName, u64), &[u8])| {
			(server, self.bucket_start(bucket), classify(value))
		},
	)
}

#[implement(super::Service)]
#[inline]
#[must_use]
fn current_bucket(&self) -> u64 {
	now_secs()
		.checked_div(self.window_secs.max(1))
		.unwrap_or(0)
}

/// Wall-clock instant at the start of `bucket`.
#[implement(super::Service)]
#[inline]
#[must_use]
fn bucket_start(&self, bucket: u64) -> SystemTime {
	let offset = bucket.saturating_mul(self.window_secs);

	UNIX_EPOCH
		.checked_add(Duration::from_secs(offset))
		.unwrap_or(UNIX_EPOCH)
}

#[implement(super::Service)]
#[inline]
#[must_use]
fn streak(&self, latest_bucket: u64, oldest_bucket: u64) -> u32 {
	let span = latest_bucket
		.saturating_sub(oldest_bucket)
		.saturating_add(1);

	u32::try_from(span)
		.unwrap_or(u32::MAX)
		.min(self.n_max)
}

/// Folds a server's failure rows into its streak, `None` when it has none.
#[implement(super::Service)]
async fn peer_streak(&self, server: &ServerName) -> Option<Streak> {
	let window_secs = self.window_secs;

	self.statuses
		.stream_prefix(&(server, Interfix))
		.ignore_err()
		.ready_fold(None, |state, ((_, bucket), value): ((&ServerName, u64), &[u8])| {
			Some(fold_streak(window_secs, state, bucket, value))
		})
		.await
}

/// Builds the pure backoff state from a server's failure streak.
#[implement(super::Service)]
fn backoff(&self, run: Streak) -> Backoff {
	Backoff {
		class: run.class,
		anchor_secs: run.anchor_secs,
		streak: self.streak(run.latest_bucket, run.oldest_bucket),
		now: now_secs(),
		window_secs: self.window_secs,
		grace_secs: self.grace.as_secs(),
	}
}

/// Projects a failure streak onto the admin-facing summary.
#[implement(super::Service)]
fn peer_backoff_from(&self, streak: Streak) -> PeerBackoff {
	PeerBackoff {
		class: streak.class,
		anchor_secs: streak.anchor_secs,
		oldest_secs: streak
			.oldest_bucket
			.saturating_mul(self.window_secs),
		delay_secs: self.backoff(streak).delay_secs(),
	}
}

/// Computes a retry verdict from a peer's latest failure state.
///
/// The peer becomes attemptable once the selected delay past the anchor has
/// elapsed. Overflow while constructing the wall-clock deadline falls back to
/// the current time.
#[must_use]
pub(super) fn attempt_verdict(backoff: &Backoff) -> ShouldAttempt {
	let earliest_secs = backoff
		.anchor_secs
		.saturating_add(backoff.delay_secs());

	if backoff.now >= earliest_secs {
		return ShouldAttempt::Yes;
	}

	ShouldAttempt::No {
		earliest_retry: UNIX_EPOCH
			.checked_add(Duration::from_secs(earliest_secs))
			.unwrap_or_else(SystemTime::now),
	}
}

impl Backoff {
	/// Calculates the bounded retry delay for this failure state.
	///
	/// Permanent failures use [`MAX_BACKOFF`]. A lone transient failure uses the
	/// configured grace tier when enabled; larger streaks follow the saturating
	/// `window * streak^2` curve and cap at the same maximum.
	#[must_use]
	pub(super) fn delay_secs(&self) -> u64 {
		let max_backoff = MAX_BACKOFF.as_secs();

		match self.class {
			| Classification::Permanent => max_backoff,
			| Classification::Transient if self.streak <= 1 && self.grace_secs != 0 =>
				self.grace_secs.min(max_backoff),
			| Classification::Transient => self
				.window_secs
				.saturating_mul(u64::from(self.streak))
				.saturating_mul(u64::from(self.streak))
				.min(max_backoff),
		}
	}
}

/// Folds one failure row into a server's running streak.
///
/// The newest row supplies the class and anchor while the first row's bucket is
/// retained as the oldest edge of the streak.
#[must_use]
pub(super) fn fold_streak(
	window_secs: u64,
	state: Option<Streak>,
	bucket: u64,
	value: &[u8],
) -> Streak {
	let anchor_secs = failure_secs(value).unwrap_or_else(|| bucket.saturating_mul(window_secs));

	let oldest_bucket = state.map_or(bucket, |streak| streak.oldest_bucket);

	Streak {
		class: classify(value),
		anchor_secs,
		oldest_bucket,
		latest_bucket: bucket,
	}
}

#[inline]
#[must_use]
/// Decodes the classification byte from a peer-status value.
///
/// Missing and unrecognized bytes are treated as transient failures for
/// compatibility with old or malformed rows.
pub(super) fn classify(bytes: &[u8]) -> Classification {
	bytes
		.first()
		.copied()
		.map_or(Classification::Transient, Classification::from_byte)
}

/// Decodes the recorded failure instant in seconds since the epoch.
///
/// Old single-byte rows and truncated values carry no timestamp and yield
/// `None`.
#[must_use]
pub(super) fn failure_secs(bytes: &[u8]) -> Option<u64> {
	bytes
		.get(1..9)
		.and_then(|tail| tail.try_into().ok())
		.map(u64::from_be_bytes)
}

/// Classifies a failed federation attempt for the peer-reachability store.
///
/// A content-level 4xx proves the peer reachable and returns `None`; 5xx, 429,
/// non-JSON responses, and transport failures are transient. A received 410 is
/// treated as a permanent proxy-level signal that the peer is gone.
#[must_use]
pub(super) fn classify_error(error: &Error) -> Option<Classification> {
	let Error::Federation(_, response) = error else {
		return Some(Classification::Transient);
	};

	let status = response.status_code;

	match status {
		| _ if status == StatusCode::GONE => Some(Classification::Permanent),
		| _ if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS =>
			Some(Classification::Transient),
		| _ if matches!(response.body, ErrorBody::NotJson { .. }) =>
			Some(Classification::Transient),
		| _ => None,
	}
}
