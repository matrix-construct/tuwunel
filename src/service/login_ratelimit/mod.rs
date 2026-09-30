//! Per-account throttles on password authentication.
//!
//! Every password attempt reserves a token from the account's failed-attempt
//! bucket before the password is checked, and only a wrong password keeps it,
//! so concurrent guesses cannot share one token. Only a sign-in by a verified
//! password debits the account bucket. Both are keyed on the account rather
//! than the client address, mirroring Synapse's `rc_login.failed_attempts` and
//! `rc_login.account`; the operator documentation for the `login_rc_*` options
//! sets out their costs.

use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
	time::{Duration, Instant},
};

use http::StatusCode;
use ruma::{
	UserId,
	api::error::{ErrorKind, LimitExceededErrorData, RetryAfter},
};
use tuwunel_core::{Error, Result, Server, implement, warn};

/// Per-account login throttles.
///
/// The tables live here rather than on [`crate::users::Service`] so that the
/// interior mutability they need stays out of a type whose `&self` methods are
/// linted as taking nothing mutable.
pub struct Service {
	server: Arc<Server>,
	account: Ratelimiter,
	failed: Ratelimiter,
}

/// A failed-attempt token reserved for one password attempt.
///
/// It carries the account key, built once per request, and whether a token was
/// actually taken. Dropping it keeps the token, as a wrong password does;
/// anything else hands it to [`Service::refund_login_attempt`] or
/// [`Service::record_login`]. A held bucket that fully refills and is pruned
/// while the password is verified, then re-created by another guess, takes
/// this reservation's refund in its place.
#[derive(Debug)]
#[must_use]
pub struct Reservation {
	key: String,
	hold: Hold,
}

/// Token-bucket table: last-refill instant and remaining tokens per account.
type Ratelimiter = Mutex<HashMap<String, (Instant, f64)>>;

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			server: args.server.clone(),
			account: Mutex::new(HashMap::new()),
			failed: Mutex::new(HashMap::new()),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Cap on the number of accounts each bucket table holds.
///
/// A full table never evicts a bucket that is still limiting; [`Axis`] decides
/// what happens to an account it cannot admit.
const RATELIMIT_MAP_CAP: usize = 1 << 16;

/// How many entries one admission inspects when the table is full.
const PRUNE_SAMPLE: usize = 64;

/// The longest `retry_after` a refusal will state, whatever the rate.
const MAX_RETRY_AFTER: Duration = Duration::from_hours(24);

/// How often each table's full-table warning may repeat while it stays full.
const FULL_WARNING_INTERVAL: Duration = Duration::from_mins(1);

static FAILED_FULL_WARNING: Mutex<Option<Instant>> = Mutex::new(None);

static ACCOUNT_FULL_WARNING: Mutex<Option<Instant>> = Mutex::new(None);

#[cfg(test)]
mod tests;

/// One bucket's configuration: refill rate per second and burst depth.
#[derive(Clone, Copy)]
struct Limit {
	rate: f64,
	burst: u32,
}

impl Limit {
	/// A zero burst, or a rate that is zero, negative or not a number, turns
	/// the bucket off rather than making it one that never refills.
	fn enabled(self) -> bool { self.burst > 0 && self.rate > 0.0 && self.rate.is_finite() }

	fn burst(self) -> f64 { f64::from(self.burst) }
}

/// Whether a reservation actually took a token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hold {
	/// A token was taken and is owed back unless a wrong password keeps it.
	Held,

	/// No token was taken: the limit is off, or a full table could not track
	/// the account.
	Untracked,
}

/// Which bucket table a debit addresses, deciding its full-table policy.
///
/// A bucket that is still limiting is never evicted to make room, since that
/// would hand the account being guessed a fresh burst.
#[derive(Clone, Copy, Debug)]
enum Axis {
	/// Wrong passwords; an account the full table cannot admit goes untracked.
	Failed,

	/// Verified sign-ins; an account the full table cannot admit is refused.
	Account,
}

impl Axis {
	fn when_full(self) -> Result<Hold> {
		match self {
			| Self::Failed => Ok(Hold::Untracked),
			| Self::Account => Err(limit_exceeded(None)),
		}
	}

	fn last_full_warning(self) -> &'static Mutex<Option<Instant>> {
		match self {
			| Self::Failed => &FAILED_FULL_WARNING,
			| Self::Account => &ACCOUNT_FULL_WARNING,
		}
	}
}

/// Reserve one failed-attempt token for a password about to be checked.
///
/// The debit is atomic, so concurrent guesses cannot all pass on one remaining
/// token, and an account with none left is refused with `M_LIMIT_EXCEEDED`.
/// Only a wrong password keeps the reservation: every other outcome hands it
/// back through [`record_login`] or [`refund_login_attempt`].
///
/// [`record_login`]: Service::record_login
/// [`refund_login_attempt`]: Service::refund_login_attempt
#[implement(Service)]
pub fn reserve_login_attempt(&self, user_id: &UserId) -> Result<Reservation> {
	reserve_at(
		&self.failed,
		account_key(user_id),
		self.failed_limit(),
		Instant::now(),
		RATELIMIT_MAP_CAP,
	)
}

/// Record a sign-in by a verified password.
///
/// The reservation is handed back first, because the password was right
/// whatever the account bucket decides. The account bucket is then debited,
/// refusing with `M_LIMIT_EXCEEDED` an account that has signed in too often.
#[implement(Service)]
pub fn record_login(&self, reservation: Reservation) -> Result {
	let Reservation { key, hold } = reservation;
	let now = Instant::now();

	refund_at(&self.failed, &key, hold, self.failed_limit(), now)?;

	debit_at(&self.account, &key, self.account_limit(), Axis::Account, now, RATELIMIT_MAP_CAP)?;

	Ok(())
}

/// Hand back a reservation whose attempt found no wrong password.
///
/// That covers a password re-entered for UIAA, which opens no session, and
/// every refusal made without checking a password at all.
#[implement(Service)]
pub fn refund_login_attempt(&self, reservation: Reservation) -> Result {
	let Reservation { key, hold } = reservation;

	refund_at(&self.failed, &key, hold, self.failed_limit(), Instant::now())
}

#[implement(Service)]
fn failed_limit(&self) -> Limit {
	let config = &self.server.config;

	Limit {
		rate: config.login_rc_failed_per_second,
		burst: config.login_rc_failed_burst_count,
	}
}

#[implement(Service)]
fn account_limit(&self) -> Limit {
	let config = &self.server.config;

	Limit {
		rate: config.login_rc_account_per_second,
		burst: config.login_rc_account_burst_count,
	}
}

/// One bucket per account regardless of the case it was typed in, matching
/// the lowercase fallback the password check itself applies.
fn account_key(user_id: &UserId) -> String { user_id.as_str().to_lowercase() }

fn reserve_at(
	table: &Ratelimiter,
	key: String,
	limit: Limit,
	now: Instant,
	cap: usize,
) -> Result<Reservation> {
	let hold = debit_at(table, &key, limit, Axis::Failed, now, cap)?;

	Ok(Reservation { key, hold })
}

/// Take one token from an account's bucket, refusing when none is left.
///
/// An account the table does not hold starts with a full bucket. When the
/// table holds `cap` accounts and nothing can be pruned, the axis decides
/// whether such an account is refused or let through untracked.
fn debit_at(
	table: &Ratelimiter,
	key: &str,
	limit: Limit,
	axis: Axis,
	now: Instant,
	cap: usize,
) -> Result<Hold> {
	if !limit.enabled() {
		return Ok(Hold::Untracked);
	}

	let Limit { rate, .. } = limit;
	let burst = limit.burst();
	let mut buckets = table.lock()?;

	debug_assert!(cap > 0, "rate-limit table cap must be positive");

	let Some(bucket) = buckets.get_mut(key) else {
		if buckets.len() >= cap {
			prune_sample(&mut buckets, rate, burst, now);
		}

		if buckets.len() >= cap {
			drop(buckets);
			warn_table_full(axis, now);
			return axis.when_full();
		}

		buckets.insert(key.to_owned(), (now, burst - 1.0));
		return Ok(Hold::Held);
	};

	let (last_time, tokens) = bucket;
	let refilled = refill(*last_time, *tokens, rate, burst, now);

	if refilled < 1.0 {
		return Err(limit_exceeded(retry_after(rate, refilled)));
	}

	*last_time = now;
	*tokens = refilled - 1.0;

	Ok(Hold::Held)
}

/// Give a held reservation's token back, never past the burst.
///
/// A bucket refunded back to full carries no information and is removed, so
/// ordinary sign-ins do not fill the table. An untracked reservation took
/// nothing and gets nothing back.
fn refund_at(table: &Ratelimiter, key: &str, hold: Hold, limit: Limit, now: Instant) -> Result {
	if hold == Hold::Untracked || !limit.enabled() {
		return Ok(());
	}

	let burst = limit.burst();
	let mut buckets = table.lock()?;

	let Some((last_time, tokens)) = buckets.get_mut(key) else {
		return Ok(());
	};

	let level = burst.min(refill(*last_time, *tokens, limit.rate, burst, now) + 1.0);

	if level >= burst {
		buckets.remove(key);
	} else {
		*last_time = now;
		*tokens = level;
	}

	Ok(())
}

fn refill(last: Instant, tokens: f64, rate: f64, burst: f64, now: Instant) -> f64 {
	now.saturating_duration_since(last)
		.as_secs_f64()
		.mul_add(rate, tokens)
		.min(burst)
}

/// Remove, from a bounded sample of the table, the buckets that have refilled
/// completely. A bucket that is still restricting is never removed.
fn prune_sample(
	buckets: &mut HashMap<String, (Instant, f64)>,
	rate: f64,
	burst: f64,
	now: Instant,
) {
	let refilled: Vec<String> = buckets
		.iter()
		.take(PRUNE_SAMPLE)
		.filter(|(_, (last, tokens))| refill(*last, *tokens, rate, burst, now) >= burst)
		.map(|(key, _)| key.clone())
		.collect();

	for key in refilled {
		buckets.remove(&key);
	}
}

/// Warn that the table for `axis` cannot admit another account.
///
/// At most once per [`FULL_WARNING_INTERVAL`] for each table, so a spray that
/// keeps one table full neither floods the log nor hides the other's warning.
fn warn_table_full(axis: Axis, now: Instant) {
	let Ok(mut last) = axis.last_full_warning().lock() else {
		return;
	};

	if last.is_some_and(|last| now.saturating_duration_since(last) < FULL_WARNING_INTERVAL) {
		return;
	}

	*last = Some(now);

	match axis {
		| Axis::Failed => warn!(
			table = ?axis,
			cap = RATELIMIT_MAP_CAP,
			"Login rate-limit table is full of accounts still being limited; wrong passwords for \
			 accounts not already in it go untracked. Likely a spray of distinct user names."
		),
		| Axis::Account => warn!(
			table = ?axis,
			cap = RATELIMIT_MAP_CAP,
			"Login rate-limit table is full of accounts still being limited; sign-ins for \
			 accounts not already in it are refused."
		),
	}
}

/// Seconds until one token is back, rounded up and capped. `None` when it
/// cannot be stated, which the error then simply omits.
fn retry_after(rate: f64, tokens: f64) -> Option<Duration> {
	let secs = ((1.0 - tokens) / rate).ceil();

	// Capped before converting: a tiny rate gives a figure too large for a
	// `Duration`, and the cap is the honest answer to it, not an omission.
	Duration::try_from_secs_f64(secs.min(MAX_RETRY_AFTER.as_secs_f64())).ok()
}

fn limit_exceeded(retry_after: Option<Duration>) -> Error {
	Error::Request(
		ErrorKind::LimitExceeded(LimitExceededErrorData {
			retry_after: retry_after.map(RetryAfter::Delay),
		}),
		"Too many login attempts for this account.".into(),
		StatusCode::TOO_MANY_REQUESTS,
	)
}
