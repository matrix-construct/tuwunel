use std::time::{Duration, Instant};

use http::StatusCode;
use ruma::{
	api::error::{ErrorKind, RetryAfter},
	user_id,
};
use tuwunel_core::{Error, Result};

use super::{
	Axis, Hold, Limit, MAX_RETRY_AFTER, Ratelimiter, Reservation, account_key, debit_at,
	refund_at, reserve_at, retry_after,
};

const CAP: usize = 16;

const KEY: &str = "@a:x";

// The default `login_rc_failed_*` limit.
const FAILED: Limit = Limit { rate: 0.17, burst: 3 };

// The default `login_rc_account_*` limit.
const ACCOUNT: Limit = Limit { rate: 0.003, burst: 5 };

#[derive(Clone, Copy, PartialEq, Eq)]
enum Password {
	Right,
	Wrong,
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
	SignedIn,
	WrongPassword,
	Limited,
}

fn at(start: Instant, secs: f64) -> Instant {
	start
		.checked_add(Duration::from_secs_f64(secs))
		.expect("test instant within range")
}

fn limit(rate: f64, burst: u32) -> Limit { Limit { rate, burst } }

fn tokens(table: &Ratelimiter, key: &str) -> f64 { table.lock().expect("locked")[key].1 }

fn holds(table: &Ratelimiter, expected: f64) -> bool {
	(tokens(table, KEY) - expected).abs() < 1e-9
}

fn tracked(table: &Ratelimiter, key: &str) -> bool {
	table.lock().expect("locked").contains_key(key)
}

fn stated_retry_after(error: &Error) -> Option<Duration> {
	match error {
		| Error::Request(ErrorKind::LimitExceeded(data), ..) =>
			data.retry_after
				.as_ref()
				.map(|retry| match retry {
					| RetryAfter::Delay(delay) => *delay,
					| RetryAfter::DateTime(_) =>
						panic!("a login refusal states a delay, not a date"),
				}),
		| other => panic!("expected M_LIMIT_EXCEEDED, got {other:?}"),
	}
}

#[test]
fn burst_then_refused_then_refilled() {
	let table = Ratelimiter::default();
	let start = Instant::now();

	drain_failed(&table, start);
	reserve(&table, start).expect_err("the attempt after the burst must be refused");

	// 0.17 tokens per second refills one token in just under six seconds.
	let _refilled = reserve(&table, at(start, 6.0))
		.expect("one token should have refilled after six seconds");
}

fn drain_failed(table: &Ratelimiter, now: Instant) -> Vec<Reservation> {
	(0..FAILED.burst)
		.map(|i| {
			reserve(table, now)
				.unwrap_or_else(|e| panic!("reservation {i} within the burst was refused: {e}"))
		})
		.collect()
}

fn reserve(table: &Ratelimiter, now: Instant) -> Result<Reservation> {
	reserve_at(table, KEY.to_owned(), FAILED, now, CAP)
}

#[test]
fn unrefunded_reservations_hold_the_burst_until_one_is_refunded() {
	let table = Ratelimiter::default();
	let now = Instant::now();

	// Unrefunded debits stand in for concurrent unverified attempts.
	let in_flight = drain_failed(&table, now);

	reserve(&table, now).expect_err("an attempt past the burst in flight is refused");

	let verified = in_flight
		.first()
		.expect("a reservation in flight");

	refund_at(&table, &verified.key, verified.hold, FAILED, now).expect("refund");

	let _next = reserve(&table, now).expect("one refund frees one attempt");

	reserve(&table, now).expect_err("and no more than one");
}

#[test]
fn refund_to_the_burst_removes_the_bucket() {
	let table = Ratelimiter::default();
	let now = Instant::now();
	let first = reserve(&table, now).expect("first reservation");
	let second = reserve(&table, now).expect("second reservation");

	refund_at(&table, &first.key, first.hold, FAILED, now).expect("refund");
	assert!(holds(&table, 2.0), "a refund returns one token");

	refund_at(&table, &second.key, second.hold, FAILED, now).expect("refund");
	assert!(!tracked(&table, KEY), "a bucket refunded back to full is removed");
}

#[test]
fn untracked_reservation_refunds_nothing() {
	let table = Ratelimiter::default();
	let now = Instant::now();
	let _guess = reserve(&table, now).expect("another attempt's reservation");

	refund_at(&table, KEY, Hold::Untracked, FAILED, now).expect("refund");
	assert!(
		holds(&table, 2.0),
		"an untracked reservation takes back no other attempt's token"
	);
}

#[test]
fn accounts_are_independent() {
	let table = Ratelimiter::default();
	let now = Instant::now();
	let one = limit(0.17, 1);

	debit_at(&table, "@a:x", one, Axis::Failed, now, CAP).expect("a: first");
	debit_at(&table, "@a:x", one, Axis::Failed, now, CAP).expect_err("a: drained");
	debit_at(&table, "@b:x", one, Axis::Failed, now, CAP)
		.expect("guessing one account must not throttle another");
}

#[test]
fn zero_burst_or_zero_rate_disables() {
	let now = Instant::now();

	for disabled in [limit(0.0, 3), limit(0.17, 0), limit(-1.0, 3), limit(f64::NAN, 3)] {
		let table = Ratelimiter::default();

		for _ in 0..100 {
			let hold = debit_at(&table, KEY, disabled, Axis::Account, now, CAP)
				.expect("a disabled bucket never refuses");

			assert_eq!(hold, Hold::Untracked, "a disabled bucket takes no token");
		}

		assert!(untouched(&table), "a disabled bucket stores nothing");
	}
}

fn untouched(table: &Ratelimiter) -> bool { table.lock().expect("locked").is_empty() }

#[test]
fn full_table_prunes_refilled_buckets() {
	let table = Ratelimiter::default();
	let start = Instant::now();
	let fast = limit(1.0, 3);

	for i in 0..CAP {
		debit_at(&table, &format!("@u{i}:x"), fast, Axis::Account, start, CAP)
			.expect("filling the table");
	}

	// One token was spent from three; at one per second, all are full again.
	debit_at(&table, "@new:x", fast, Axis::Account, at(start, 2.0), CAP)
		.expect("a refilled bucket is pruned to admit a new account");

	let buckets = table.lock().expect("locked");
	assert!(buckets.len() <= CAP, "the table stays within its cap");
	assert!(buckets.contains_key("@new:x"), "the new account was admitted");
}

#[test]
fn full_account_table_never_evicts_a_limiting_bucket() {
	let table = Ratelimiter::default();
	let start = Instant::now();
	let slow = limit(0.003, 1);

	// The victim's bucket is drained and is the oldest in the table.
	debit_at(&table, "@victim:x", slow, Axis::Account, start, CAP).expect("victim's one try");

	for i in 1..CAP {
		debit_at(&table, &format!("@spray{i}:x"), slow, Axis::Account, at(start, 1.0), CAP)
			.expect("filling the table");
	}

	let refusal = debit_at(&table, "@one-more:x", slow, Axis::Account, at(start, 2.0), CAP)
		.expect_err("a full account table of limiting buckets refuses a new account");

	assert!(
		stated_retry_after(&refusal).is_none(),
		"a full-table refusal states no delay it cannot know"
	);

	debit_at(&table, "@victim:x", slow, Axis::Account, at(start, 2.0), CAP)
		.expect_err("the spray must not have reset the victim's limit");

	assert!(tracked(&table, "@victim:x"), "the victim's bucket is still in the table");
}

#[test]
fn full_failed_table_lets_an_untracked_account_through() {
	let table = Ratelimiter::default();
	let start = Instant::now();
	let slow = limit(0.003, 1);
	let key = "@one-more:x";

	for i in 0..CAP {
		debit_at(&table, &format!("@spray{i}:x"), slow, Axis::Failed, start, CAP)
			.expect("filling the table");
	}

	for _ in 0..3 {
		let hold = debit_at(&table, key, slow, Axis::Failed, at(start, 1.0), CAP)
			.expect("a full failed table lets an account it cannot hold through");

		assert_eq!(hold, Hold::Untracked, "the account let through took no token");
	}

	assert!(!tracked(&table, key), "the account let through is not tracked");
}

#[test]
fn wrong_passwords_past_the_failed_burst_are_refused() {
	let failed = Ratelimiter::default();
	let account = Ratelimiter::default();
	let start = Instant::now();

	for i in 0..FAILED.burst {
		assert_eq!(
			attempt(&failed, &account, Password::Wrong, start),
			Outcome::WrongPassword,
			"wrong password {i} within the failed burst is checked"
		);
	}

	assert_eq!(
		attempt(&failed, &account, Password::Wrong, start),
		Outcome::Limited,
		"a wrong password past the failed burst is refused"
	);

	assert_eq!(
		attempt(&failed, &account, Password::Right, start),
		Outcome::Limited,
		"a drained failed axis refuses the correct password too"
	);

	assert!(
		untouched(&account),
		"nothing refused before verification debits the account axis"
	);

	// 0.17 tokens per second refills one token in just under six seconds.
	assert_eq!(
		attempt(&failed, &account, Password::Right, at(start, 6.0)),
		Outcome::SignedIn,
		"the correct password gets in once the failed axis refills"
	);
}

/// One password attempt through the calls every password sign-in makes.
///
/// It reserves a failed-attempt token before verifying, keeps it for a wrong
/// password, and for a right one refunds it and then debits the account axis,
/// as `record_login` does.
fn attempt(
	failed: &Ratelimiter,
	account: &Ratelimiter,
	password: Password,
	now: Instant,
) -> Outcome {
	let Ok(reservation) = reserve(failed, now) else {
		return Outcome::Limited;
	};

	if password == Password::Wrong {
		return Outcome::WrongPassword;
	}

	refund_at(failed, &reservation.key, reservation.hold, FAILED, now).expect("refund");

	debit_at(account, KEY, ACCOUNT, Axis::Account, now, CAP)
		.map_or(Outcome::Limited, |_| Outcome::SignedIn)
}

#[test]
fn wrong_passwords_do_not_debit_the_account_bucket() {
	let failed = Ratelimiter::default();
	let account = Ratelimiter::default();
	let start = Instant::now();

	// Six seconds apart, each wrong password finds a refilled failed token.
	for i in 0..ACCOUNT.burst * 2 {
		assert_eq!(
			attempt(&failed, &account, Password::Wrong, at(start, f64::from(i) * 6.0)),
			Outcome::WrongPassword,
			"wrong password {i}, paced by the failed axis, is checked"
		);
	}

	assert!(untouched(&account), "wrong passwords leave the account axis untouched");

	assert_eq!(
		attempt(&failed, &account, Password::Right, at(start, 60.0)),
		Outcome::SignedIn,
		"the owner still signs in after more wrong passwords than the account burst"
	);

	assert!(holds(&account, 4.0), "only the sign-in debited the account axis");
}

#[test]
fn correct_logins_past_the_account_burst_are_refused() {
	let failed = Ratelimiter::default();
	let account = Ratelimiter::default();
	let start = Instant::now();

	for i in 0..ACCOUNT.burst {
		assert_eq!(
			attempt(&failed, &account, Password::Right, start),
			Outcome::SignedIn,
			"sign-in {i} within the account burst succeeds"
		);
	}

	assert_eq!(
		attempt(&failed, &account, Password::Right, start),
		Outcome::Limited,
		"a sign-in past the account burst is refused although the password is right"
	);

	assert!(
		!tracked(&failed, KEY),
		"every correct password, the refused one included, got its reservation back"
	);

	// 0.003 tokens per second refills one token in just under 334 seconds.
	assert_eq!(
		attempt(&failed, &account, Password::Right, at(start, 334.0)),
		Outcome::SignedIn,
		"a sign-in succeeds again once the account axis refills"
	);
}

#[test]
fn case_variants_share_one_bucket() {
	assert_eq!(
		account_key(user_id!("@Mike:example.org")),
		account_key(user_id!("@mike:example.org")),
		"an account typed in another case must not get a fresh bucket"
	);
}

#[test]
fn refusal_states_when_to_retry() {
	let table = Ratelimiter::default();
	let now = Instant::now();
	let half = limit(0.5, 1);

	debit_at(&table, KEY, half, Axis::Account, now, CAP).expect("first");

	let error =
		debit_at(&table, KEY, half, Axis::Account, now, CAP).expect_err("second is refused");

	assert_eq!(error.status_code(), StatusCode::TOO_MANY_REQUESTS);
	assert_eq!(
		stated_retry_after(&error),
		Some(Duration::from_secs(2)),
		"at half a token per second, the next token is two seconds away"
	);
}

#[test]
fn retry_after_is_capped_and_never_panics() {
	assert_eq!(retry_after(f64::MIN_POSITIVE, 0.0), Some(MAX_RETRY_AFTER));
	assert_eq!(retry_after(1e-12, 0.0), Some(MAX_RETRY_AFTER));
	assert_eq!(retry_after(0.17, 0.0), Some(Duration::from_secs(6)));
}
