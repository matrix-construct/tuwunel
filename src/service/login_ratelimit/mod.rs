//! Per-account throttles on password authentication.
//!
//! Two token buckets keyed on the account being tried, mirroring Synapse's
//! `rc_login.account` and `rc_login.failed_attempts`:
//!
//! - **account** — every password attempt against the account, successful or
//!   not, debits one token before the password is checked.
//! - **failed** — only a wrong password debits; the bucket is consulted before
//!   the check, so an account that has run out refuses even a correct
//!   password until it refills.
//!
//! Both key on the account rather than on the caller's IP. A homeserver behind
//! a reverse proxy or tunnel that does not forward the client address sees
//! every request arrive from one IP, and a per-IP limit there either never
//! trips or throttles every user at once. A per-account limit bounds guessing
//! against one account whatever address the guesses come from. Unknown
//! accounts are throttled exactly like real ones, so the `429` is not an oracle
//! for which accounts exist.
//!
//! # What this costs
//!
//! **An attacker who keeps guessing an account's password keeps its owner from
//! signing in with that password.** While the buckets are drained, a correct
//! password is refused with `M_LIMIT_EXCEEDED` just like a wrong one; that is
//! what makes the limit a limit. It does not sign anybody out and does not
//! touch any path that is not a password: existing sessions keep working, and
//! token login (`login_via_existing_session`), SSO and JWT are not counted.
//! The owner is locked out of *password* sign-in for as long as the guessing
//! continues, and no longer. Synapse makes the same trade with the same
//! defaults.
//!
//! **A second cost, paid only under a spray of distinct names:** once the
//! table is full of accounts still being limited, password sign-in is refused
//! for every account *not already in it* — see "When the table is full". At
//! the default rates a sprayed entry refills and becomes prunable about five
//! and a half minutes after its last attempt, so this lasts about that long
//! after the spray stops. Accounts already in the table are unaffected. The
//! alternative, evicting a bucket that is still limiting, would let the same
//! spray reset the limit on the account actually being guessed.
//!
//! # Defaults are Synapse's
//!
//! The four defaults are Synapse's own, deliberately, so a server moving
//! between the two implementations keeps the same behaviour and none of the
//! numbers is ours to defend:
//!
//! | Key                            | Default | Synapse equivalent                     |
//! |--------------------------------|---------|----------------------------------------|
//! | `login_rc_account_per_second`  | 0.003   | `rc_login.account.per_second`          |
//! | `login_rc_account_burst_count` | 5       | `rc_login.account.burst_count`         |
//! | `login_rc_failed_per_second`   | 0.17    | `rc_login.failed_attempts.per_second`  |
//! | `login_rc_failed_burst_count`  | 3       | `rc_login.failed_attempts.burst_count` |
//!
//! That is five attempts, then about one every five and a half minutes, per
//! account; and three wrong passwords, then about one attempt every six
//! seconds. Source:
//! <https://element-hq.github.io/synapse/latest/usage/configuration/config_documentation.html#rc_login>
//!
//! Setting a `*_burst_count` to `0`, or a `*_per_second` to `0`, disables
//! that bucket. A rate of `0` with a non-zero burst would otherwise mean a
//! bucket that never refills — one wrong password locking an account out of
//! password login until restart — which is never what the setting meant.
//!
//! # When the table is full
//!
//! Each table holds at most [`RATELIMIT_MAP_CAP`] accounts. Past that, a new
//! account is admitted only by pruning a bucket that has refilled completely
//! and so no longer restricts anybody. **A bucket that is still restricting is
//! never evicted**, because evicting it is exactly what an attacker spraying
//! distinct names would want: it would hand the account they are guessing a
//! fresh burst. So when nothing can be pruned, the table **fails closed for
//! accounts it does not yet hold** — they are refused as though limited, and a
//! warning is logged — while every account already in the table carries on.
//! Pruning looks at a bounded sample, never the whole table, so a full table
//! costs every login a constant amount of work under the lock, not a scan.

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

/// Cap on each bucket table. See the module's "When the table is full".
const RATELIMIT_MAP_CAP: usize = 1 << 16;

/// How many entries one admission inspects when the table is full.
const PRUNE_SAMPLE: usize = 64;

/// The longest `retry_after` a refusal will state, whatever the rate.
const MAX_RETRY_AFTER: Duration = Duration::from_hours(24);

/// How often the full-table warning may repeat while the table stays full.
const FULL_WARNING_INTERVAL: Duration = Duration::from_mins(1);

static LAST_FULL_WARNING: Mutex<Option<Instant>> = Mutex::new(None);

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
}

/// Gate a `/login`-style password attempt: refuse if the failed-attempt bucket
/// is empty, then debit the account bucket.
///
/// Call before verifying the password; call [`record_failed_login`] after a
/// wrong one.
///
/// [`record_failed_login`]: Service::record_failed_login
#[implement(Service)]
pub fn check_login_rate_limit(&self, user_id: &UserId) -> Result {
	let config = &self.server.config;

	check_login_at(
		&self.failed,
		&self.account,
		&account_key(user_id),
		Limit {
			rate: config.login_rc_failed_per_second,
			burst: config.login_rc_failed_burst_count,
		},
		Limit {
			rate: config.login_rc_account_per_second,
			burst: config.login_rc_account_burst_count,
		},
		Instant::now(),
		RATELIMIT_MAP_CAP,
	)
}

/// Gate a password re-entry for an already signed-in user (UIAA): refuse if
/// the failed-attempt bucket is empty, without debiting the account bucket.
///
/// The caller holds a valid access token, so the account axis — which exists
/// to bound anonymous guessing — does not apply; the failed axis does, because
/// a stolen token must not become an unlimited password oracle.
#[implement(Service)]
pub fn check_failed_login_rate_limit(&self, user_id: &UserId) -> Result {
	let config = &self.server.config;

	check_bucket_at(
		&self.failed,
		&account_key(user_id),
		Limit {
			rate: config.login_rc_failed_per_second,
			burst: config.login_rc_failed_burst_count,
		},
		Instant::now(),
		RATELIMIT_MAP_CAP,
		Debit::No,
	)
}

/// Record a wrong password against the account's failed-attempt bucket.
#[implement(Service)]
pub fn record_failed_login(&self, user_id: &UserId) {
	let config = &self.server.config;

	// The refusal this can return is for the next attempt; this one has
	// already failed on its own account.
	check_bucket_at(
		&self.failed,
		&account_key(user_id),
		Limit {
			rate: config.login_rc_failed_per_second,
			burst: config.login_rc_failed_burst_count,
		},
		Instant::now(),
		RATELIMIT_MAP_CAP,
		Debit::Yes,
	)
	.ok();
}

/// One bucket per account regardless of the case it was typed in, matching
/// the lowercase fallback the password check itself applies.
fn account_key(user_id: &UserId) -> String { user_id.as_str().to_lowercase() }

/// The two-bucket gate behind [`check_login_rate_limit`], clock and cap
/// explicit so it can be tested: the failed axis is only peeked, the account
/// axis is debited, and the account axis is not touched when the failed one
/// refuses.
///
/// [`check_login_rate_limit`]: Service::check_login_rate_limit
fn check_login_at(
	failed_table: &Ratelimiter,
	account_table: &Ratelimiter,
	key: &str,
	failed: Limit,
	account: Limit,
	now: Instant,
	cap: usize,
) -> Result {
	check_bucket_at(failed_table, key, failed, now, cap, Debit::No)?;
	check_bucket_at(account_table, key, account, now, cap, Debit::Yes)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Debit {
	Yes,
	No,
}

fn check_bucket_at(
	table: &Ratelimiter,
	key: &str,
	limit: Limit,
	now: Instant,
	cap: usize,
	debit: Debit,
) -> Result {
	if !limit.enabled() {
		return Ok(());
	}

	let Limit { rate, .. } = limit;
	let burst = f64::from(limit.burst);
	let mut buckets = table.lock()?;
	debug_assert!(cap > 0, "rate-limit table cap must be positive");

	let Some(bucket) = buckets.get_mut(key) else {
		// An untouched account has a full bucket; a peek needs no entry.
		if debit == Debit::No {
			return Ok(());
		}

		if buckets.len() >= cap {
			prune_sample(&mut buckets, rate, burst, now);
		}

		if buckets.len() >= cap {
			drop(buckets);
			warn_table_full(now);
			return Err(limit_exceeded(None));
		}

		buckets.insert(key.to_owned(), (now, burst - 1.0));
		return Ok(());
	};

	let (last_time, tokens) = bucket;
	let refilled = refill(*last_time, *tokens, rate, burst, now);

	if refilled < 1.0 {
		return Err(limit_exceeded(retry_after(rate, refilled)));
	}

	if debit == Debit::Yes {
		*last_time = now;
		*tokens = refilled - 1.0;
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

/// At most once per [`FULL_WARNING_INTERVAL`], so a spray that keeps the table
/// full does not also flood the log.
fn warn_table_full(now: Instant) {
	let Ok(mut last) = LAST_FULL_WARNING.lock() else {
		return;
	};

	if last.is_some_and(|last| now.saturating_duration_since(last) < FULL_WARNING_INTERVAL) {
		return;
	}

	*last = Some(now);
	warn!(
		cap = RATELIMIT_MAP_CAP,
		"Login rate-limit table is full of accounts still being limited; refusing password \
		 logins for accounts not already in it. Likely a spray of distinct user names."
	);
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
