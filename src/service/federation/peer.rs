//! Per-server reachability store backed by the `servername_status` CF.
//!
//! Each failure writes one row keyed `(servername, bucket)` with
//! `bucket = now.as_secs() / window_secs`; the tuple codec joins the parts with
//! `ser::SEP`, so the on-disk key is `servername || SEP || u64_be(bucket)`. The
//! value is the [`Classification`] byte, optionally trailed by the failure
//! instant as `u64_be` seconds. Two failures in one window collide on the same
//! key (a correct collision: the window is the coalescing quantum) and two
//! failures in different windows produce two rows, so a failure is always a
//! blind write and never a read-modify-write.
//!
//! `should_attempt` scans a server's rows: the newest failure is the backoff
//! anchor (its recorded instant) and the window span between the oldest and
//! newest surviving rows is the streak, so the gate and the `earliest_retry`
//! it reports are one comparison and stay coherent when the clock crosses a
//! window boundary. `record_success` and `note_peer_alive` clear the whole
//! prefix, so a recovered or reachable peer is immediately attemptable again.
//!
//! `window_secs` is sourced from `sender_timeout` at service build time so the
//! peer-status curve does not drift from the sender's existing quadratic
//! backoff when both observe the same peer.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{Stream, StreamExt};
use http::StatusCode;
use ruma::ServerName;
use tuwunel_core::{
	Error, implement,
	utils::{
		stream::{ReadyExt, TryIgnore},
		time::now_secs,
	},
};
use tuwunel_database::Interfix;

/// Backoff ceiling, matching `sender_retry_backoff_limit`'s 24h default.
pub(super) const MAX_BACKOFF: Duration = Duration::from_hours(24);

/// Permanence classification supplied alongside a failure.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Classification {
	#[default]
	Transient,
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

/// Verdict for [`Service::should_attempt`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShouldAttempt {
	Yes,
	No {
		earliest_retry: SystemTime,
	},

	/// Eligible but should be sorted to the back of any candidate list
	/// rather than skipped outright.
	#[allow(dead_code)]
	Deprioritize,
}

/// Latest-failure state feeding the pure [`attempt_verdict`] decision.
pub(super) struct Backoff {
	pub(super) class: Classification,

	/// Failure instant the delay is measured from (seconds since the epoch).
	pub(super) anchor_secs: u64,

	pub(super) streak: u32,

	/// Current time (seconds since the epoch); injected for testability.
	pub(super) now: u64,

	pub(super) window_secs: u64,
	pub(super) grace_secs: u64,
}

#[implement(super::Service)]
pub async fn record_success(&self, server: &ServerName) {
	self.statuses
		.del_prefix(&(server, Interfix))
		.await;
}

/// Clears a peer's failure rows after it has proven reachable via inbound
/// activity, reporting whether any were present so the caller flushes only for
/// a peer that was actually sad. The healthy-peer miss writes no tombstone.
#[implement(super::Service)]
#[tracing::instrument(
	level = "trace",
	skip(self),
	fields(
		%server,
	),
)]
pub async fn note_peer_alive(&self, server: &ServerName) -> bool {
	let prefix = (server, Interfix);
	let sad = self
		.statuses
		.stream_prefix_raw(&prefix)
		.ignore_err()
		.ready_any(|_| true)
		.await;

	if sad {
		self.statuses.del_prefix(&prefix).await;
	}

	sad
}

#[implement(super::Service)]
pub fn record_failure(&self, server: &ServerName, classification: Classification) {
	// Raw-value additive extension; old one-byte rows stay readable.
	let mut value = [0_u8; 9];
	value[0] = u8::from(classification);
	value[1..].copy_from_slice(&now_secs().to_be_bytes());

	self.statuses
		.put_raw((server, self.current_bucket()), value);
}

#[implement(super::Service)]
#[tracing::instrument(skip(self), fields(%server), level = "trace")]
pub async fn should_attempt(&self, server: &ServerName) -> ShouldAttempt {
	let window_secs = self.window_secs;
	let latest = self
		.statuses
		.stream_prefix(&(server, Interfix))
		.ignore_err()
		.ready_fold(None, |state, ((_, bucket), value): ((&ServerName, u64), &[u8])| {
			let anchor_secs =
				failure_secs(value).unwrap_or_else(|| bucket.saturating_mul(window_secs));

			let oldest_bucket = state.map_or(bucket, |(_, _, oldest, _)| oldest);

			Some((classify(value), anchor_secs, oldest_bucket, bucket))
		})
		.await;

	let Some((class, anchor_secs, oldest_bucket, latest_bucket)) = latest else {
		return ShouldAttempt::Yes;
	};

	attempt_verdict(&Backoff {
		class,
		anchor_secs,
		streak: self.streak(latest_bucket, oldest_bucket),
		now: now_secs(),
		window_secs,
		grace_secs: self.grace.as_secs(),
	})
}

/// Yields one tuple per populated bucket, ordered by `(server, bucket_start)`,
/// backing the admin `peer-status snapshot` table.
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

/// Pure backoff verdict from a peer's latest failure state. `Permanent` and a
/// saturating `window * streak^2` curve both cap at [`MAX_BACKOFF`]; a lone
/// `Transient` failure gets the `grace` tier when it is enabled.
#[must_use]
pub(super) fn attempt_verdict(backoff: &Backoff) -> ShouldAttempt {
	let &Backoff {
		class,
		anchor_secs,
		streak,
		now,
		window_secs,
		grace_secs,
	} = backoff;

	let max_backoff = MAX_BACKOFF.as_secs();
	let delay = match class {
		| Classification::Permanent => max_backoff,
		| Classification::Transient if streak <= 1 && grace_secs != 0 =>
			grace_secs.min(max_backoff),
		| Classification::Transient => window_secs
			.saturating_mul(u64::from(streak))
			.saturating_mul(u64::from(streak))
			.min(max_backoff),
	};

	let earliest_secs = anchor_secs.saturating_add(delay);

	if now >= earliest_secs {
		return ShouldAttempt::Yes;
	}

	ShouldAttempt::No {
		earliest_retry: UNIX_EPOCH
			.checked_add(Duration::from_secs(earliest_secs))
			.unwrap_or_else(SystemTime::now),
	}
}

#[inline]
#[must_use]
pub(super) fn classify(bytes: &[u8]) -> Classification {
	bytes
		.first()
		.copied()
		.map_or(Classification::Transient, Classification::from_byte)
}

/// Failure instant (seconds since the epoch) recorded after the classification
/// byte; old single-byte rows carry no timestamp and yield `None`.
#[must_use]
pub(super) fn failure_secs(bytes: &[u8]) -> Option<u64> {
	bytes
		.get(1..9)
		.and_then(|tail| tail.try_into().ok())
		.map(u64::from_be_bytes)
}

/// Classifies a failed federation attempt for the peer-reachability store, or
/// `None` when it carries no reachability signal. An HTTP response proves the
/// peer reachable, so a content-level 4xx (a forbidden invite, a 403 backfill)
/// must not count against it; only 5xx or an explicit rate-limit (429) records
/// `Transient`. A 410 is the exception: a Matrix server never returns it for
/// one endpoint and not another, so a received 410 is a proxy operator
/// deliberately signaling the peer is gone, and records `Permanent`. Transport
/// failures carry no response and are always transient.
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
		| _ => None,
	}
}
