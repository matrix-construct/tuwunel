//! Password throttles by account and client address.
//!
//! Each attempt spends an address token and reserves an account/address failure
//! token before verification; only a wrong password keeps the reservation.
//! Verified sign-ins also spend an account token, while UIAA uses separate
//! attempt tables so public login traffic cannot exhaust its allowance.

use std::{
	borrow::Borrow,
	collections::BTreeMap,
	net::{IpAddr, Ipv6Addr},
	ops::Bound::{Excluded, Included, Unbounded},
	sync::{Arc, Mutex},
	time::{Duration, Instant},
};

use http::StatusCode;
use ruma::{
	UserId,
	api::error::{ErrorKind, LimitExceededErrorData, RetryAfter},
};
use tuwunel_core::{
	Error, Result, Server, implement, smallstr::SmallString, smallvec::SmallVec, warn,
};

/// Password login and reauthentication throttles.
///
/// The tables live here rather than on [`crate::users::Service`] so that the
/// interior mutability they need stays out of a type whose `&self` methods are
/// linted as taking nothing mutable.
pub struct Service {
	server: Arc<Server>,
	account: Ratelimiter<AccountKey>,
	login: Attempts,
	uiaa: Attempts,
}

/// A failed-attempt token reserved for one password attempt.
///
/// Dropping it keeps the token, as a wrong password does; other outcomes pass it
/// to [`Service::refund_login_attempt`] or [`Service::record_login`]. The scope
/// keeps public login refunds separate from UIAA refunds.
#[derive(Debug)]
#[must_use]
pub struct Reservation {
	key: FailedKey,
	hold: Hold,
	scope: Scope,
}

type AccountKey = SmallString<[u8; 48]>;
type FailedKey = (AccountKey, IpAddr);
type Ratelimiter<K = String> = Mutex<Table<K>>;
type Buckets<K> = BTreeMap<K, (Instant, f64)>;
type Pruned<K> = SmallVec<[K; 1]>;

#[derive(Default)]
struct Attempts {
	/// Refilled buckets expire on admission or refund; each table holds at most 65536 keys.
	failed: Ratelimiter<FailedKey>,

	/// Refilled buckets expire on admission; each table holds at most 65536 addresses.
	address: Ratelimiter<IpAddr>,
}

struct Table<K = String> {
	buckets: Buckets<K>,
	cursor: Option<K>,
}

#[derive(Clone, Copy, Debug)]
enum Scope {
	Login,
	Uiaa,
}

impl<K> Default for Table<K> {
	fn default() -> Self { Self { buckets: BTreeMap::new(), cursor: None } }
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			server: args.server.clone(),
			account: Ratelimiter::default(),
			login: Attempts::default(),
			uiaa: Attempts::default(),
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

static ADDRESS_FULL_WARNING: Mutex<Option<Instant>> = Mutex::new(None);

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

	/// All password attempts by address; a full table admits untracked addresses.
	Address,

	/// Verified sign-ins; an account the full table cannot admit is refused.
	Account,
}

impl Axis {
	fn when_full(self) -> Result<Hold> {
		match self {
			| Self::Failed | Self::Address => Ok(Hold::Untracked),
			| Self::Account => Err(limit_exceeded(None)),
		}
	}

	fn last_full_warning(self) -> &'static Mutex<Option<Instant>> {
		match self {
			| Self::Failed => &FAILED_FULL_WARNING,
			| Self::Address => &ADDRESS_FULL_WARNING,
			| Self::Account => &ACCOUNT_FULL_WARNING,
		}
	}
}

/// Reserve an account/address failure token before checking a login password.
///
/// The address budget is spent first and is never refunded. Atomic reservations
/// prevent simultaneous guesses from sharing the last failure token.
#[implement(Service)]
pub fn reserve_login_attempt(&self, user_id: &UserId, address: IpAddr) -> Result<Reservation> {
	self.reserve_attempt(user_id, address, Scope::Login)
}

/// Reserve a password reauthentication attempt independently of public login.
///
/// Both the address and failed-attempt budgets are separate from sign-in budgets.
#[implement(Service)]
pub fn reserve_uiaa_attempt(&self, user_id: &UserId, address: IpAddr) -> Result<Reservation> {
	self.reserve_attempt(user_id, address, Scope::Uiaa)
}

#[implement(Service)]
fn reserve_attempt(
	&self,
	user_id: &UserId,
	address: IpAddr,
	scope: Scope,
) -> Result<Reservation> {
	reserve_at(
		self.attempts(scope),
		(user_id, address),
		(self.failed_limit(), self.address_limit()),
		scope,
		Instant::now(),
	)
}

#[implement(Service)]
fn attempts(&self, scope: Scope) -> &Attempts {
	match scope {
		| Scope::Login => &self.login,
		| Scope::Uiaa => &self.uiaa,
	}
}

/// Record a sign-in by a verified password.
///
/// Refund its failure token before spending the account's successful-login
/// allowance; a correct password never consumes a failed-attempt token.
#[implement(Service)]
pub fn record_login(&self, reservation: Reservation) -> Result {
	let Reservation { key, hold, scope } = reservation;
	let now = Instant::now();

	refund_at(&self.attempts(scope).failed, &key, hold, self.failed_limit(), now)?;
	debit_at(
		&self.account,
		&key.0,
		self.account_limit(),
		Axis::Account,
		now,
		RATELIMIT_MAP_CAP,
	)?;

	Ok(())
}

/// Refund an attempt that found no wrong password.
///
/// The reservation selects its original login or UIAA failure table; the
/// address budget still counts the request.
#[implement(Service)]
pub fn refund_login_attempt(&self, reservation: Reservation) -> Result {
	let Reservation { key, hold, scope } = reservation;

	refund_at(&self.attempts(scope).failed, &key, hold, self.failed_limit(), Instant::now())
}

#[implement(Service)]
fn failed_limit(&self) -> Limit {
	let failed = &self.server.config.rate_limiting.login.failed;

	Limit {
		rate: failed.per_second,
		burst: failed.burst_count,
	}
}

#[implement(Service)]
fn account_limit(&self) -> Limit {
	let account = &self.server.config.rate_limiting.login.account;

	Limit {
		rate: account.per_second,
		burst: account.burst_count,
	}
}

#[implement(Service)]
fn address_limit(&self) -> Limit {
	let address = &self.server.config.rate_limiting.login.address;

	Limit {
		rate: address.per_second,
		burst: address.burst_count,
	}
}

fn reserve_at(
	attempts: &Attempts,
	(user_id, address): (&UserId, IpAddr),
	(failed, limit): (Limit, Limit),
	scope: Scope,
	now: Instant,
) -> Result<Reservation> {
	let address = address_key(address);

	debit_at(&attempts.address, &address, limit, Axis::Address, now, RATELIMIT_MAP_CAP)?;

	let key = (account_key(user_id), address);
	let hold = debit_at(&attempts.failed, &key, failed, Axis::Failed, now, RATELIMIT_MAP_CAP)?;

	Ok(Reservation { key, hold, scope })
}

fn address_key(address: IpAddr) -> IpAddr {
	match address.to_canonical() {
		| IpAddr::V4(address) => address.into(),
		| IpAddr::V6(address) => {
			let [a, b, c, d, ..] = address.segments();

			Ipv6Addr::new(a, b, c, d, 0, 0, 0, 0).into()
		},
	}
}

fn account_key(user_id: &UserId) -> AccountKey {
	let account = user_id.as_str();

	if account.is_ascii() {
		account
			.chars()
			.map(|ch| ch.to_ascii_lowercase())
			.collect()
	} else {
		AccountKey::from_str(&account.to_lowercase())
	}
}

/// Take one token from an account's bucket, refusing when none is left.
///
/// An account the table does not hold starts with a full bucket. When the
/// table holds `cap` accounts and nothing can be pruned, the axis decides
/// whether such an account is refused or let through untracked.
fn debit_at<K, Q>(
	table: &Ratelimiter<K>,
	key: &Q,
	limit: Limit,
	axis: Axis,
	now: Instant,
	cap: usize,
) -> Result<Hold>
where
	K: Borrow<Q> + Clone + Ord,
	Q: Ord + ToOwned<Owned = K> + ?Sized,
{
	if !limit.enabled() {
		return Ok(Hold::Untracked);
	}

	let Limit { rate, .. } = limit;
	let burst = limit.burst();
	let mut table = table.lock()?;
	let Table { buckets, cursor } = &mut *table;

	debug_assert!(cap > 0, "rate-limit table cap must be positive");

	let Some(bucket) = buckets.get_mut(key) else {
		if buckets.len() >= cap {
			prune_sample(buckets, cursor, rate, burst, now);
		}

		if buckets.len() >= cap {
			drop(table);
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
fn refund_at<K, Q>(
	table: &Ratelimiter<K>,
	key: &Q,
	hold: Hold,
	limit: Limit,
	now: Instant,
) -> Result
where
	K: Borrow<Q> + Ord,
	Q: Ord + ?Sized,
{
	if hold == Hold::Untracked || !limit.enabled() {
		return Ok(());
	}

	let burst = limit.burst();
	let mut table = table.lock()?;
	let Table { buckets, .. } = &mut *table;

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

/// Remove, from the next bounded sample of the table, the buckets that have
/// refilled completely.
///
/// Each sample starts after the last account the previous one inspected and
/// wraps around at the end, so successive samples reach every bucket even
/// while the leading ones stay restricted. A bucket that is still restricted
/// is never removed.
fn prune_sample<K: Clone + Ord>(
	buckets: &mut Buckets<K>,
	cursor: &mut Option<K>,
	rate: f64,
	burst: f64,
	now: Instant,
) {
	let after = (cursor.as_ref().map_or(Unbounded, Excluded), Unbounded);
	let wrapped = (Unbounded, cursor.as_ref().map_or(Unbounded, Included));
	let sample = buckets
		.range::<K, _>(after)
		.chain(
			buckets
				.range::<K, _>(wrapped)
				.take(if cursor.is_some() { PRUNE_SAMPLE } else { 0 }),
		)
		.take(PRUNE_SAMPLE);

	if let Some((key, _)) = sample.clone().last() {
		match cursor {
			| None => *cursor = Some(key.clone()),
			| Some(cursor) => cursor.clone_from(key),
		}
	}

	let refilled: Pruned<K> = sample
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
		| Axis::Failed | Axis::Address => warn!(
			table = ?axis,
			cap = RATELIMIT_MAP_CAP,
			"Password rate-limit table is full of keys still being limited; attempts for \
			 keys not already in it go untracked."
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
		"Too many password authentication attempts.".into(),
		StatusCode::TOO_MANY_REQUESTS,
	)
}
