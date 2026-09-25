use std::time::{Duration, Instant};

use ruma::{api::error::ErrorKind, user_id};
use tuwunel_core::Error;

use super::{
	Debit, Limit, MAX_RETRY_AFTER, Ratelimiter, account_key, check_bucket_at, check_login_at,
	retry_after,
};

const CAP: usize = 16;

fn at(start: Instant, secs: f64) -> Instant {
	start
		.checked_add(Duration::from_secs_f64(secs))
		.expect("test instant within range")
}

fn limit(rate: f64, burst: u32) -> Limit { Limit { rate, burst } }

fn tokens(table: &Ratelimiter, key: &str) -> f64 { table.lock().expect("locked")[key].1 }

fn stated_retry_after(error: &Error) -> Option<Duration> {
	match error {
		| Error::Request(ErrorKind::LimitExceeded(data), ..) =>
			data.retry_after
				.as_ref()
				.map(|retry| match retry {
					| ruma::api::error::RetryAfter::Delay(delay) => *delay,
					| ruma::api::error::RetryAfter::DateTime(_) =>
						panic!("a login refusal states a delay, not a date"),
				}),
		| other => panic!("expected M_LIMIT_EXCEEDED, got {other:?}"),
	}
}

#[test]
fn burst_then_refused_then_refilled() {
	let table = Ratelimiter::default();
	let start = Instant::now();

	for i in 0..3 {
		check_bucket_at(&table, "@a:x", limit(0.17, 3), at(start, 0.0), CAP, Debit::Yes)
			.unwrap_or_else(|e| panic!("attempt {i} within the burst was refused: {e}"));
	}

	check_bucket_at(&table, "@a:x", limit(0.17, 3), at(start, 0.0), CAP, Debit::Yes)
		.expect_err("the attempt after the burst must be refused");

	// 0.17 tokens per second refills one token in just under six seconds.
	check_bucket_at(&table, "@a:x", limit(0.17, 3), at(start, 6.0), CAP, Debit::Yes)
		.expect("one token should have refilled after six seconds");
}

#[test]
fn peek_never_debits() {
	let table = Ratelimiter::default();
	let now = Instant::now();

	for _ in 0..10 {
		check_bucket_at(&table, "@a:x", limit(1.0, 1), now, CAP, Debit::No)
			.expect("a peek must never drain the bucket");
	}

	assert!(
		table.lock().expect("locked").is_empty(),
		"a peek at an untouched account must not create a bucket"
	);
}

#[test]
fn drained_failed_bucket_refuses_a_peek() {
	let table = Ratelimiter::default();
	let now = Instant::now();

	check_bucket_at(&table, "@a:x", limit(0.17, 1), now, CAP, Debit::Yes)
		.expect("first failure recorded");

	check_bucket_at(&table, "@a:x", limit(0.17, 1), now, CAP, Debit::No)
		.expect_err("a drained failed-attempt bucket must refuse even a correct password");
}

#[test]
fn accounts_are_independent() {
	let table = Ratelimiter::default();
	let now = Instant::now();

	check_bucket_at(&table, "@a:x", limit(0.17, 1), now, CAP, Debit::Yes).expect("a: first");
	check_bucket_at(&table, "@a:x", limit(0.17, 1), now, CAP, Debit::Yes)
		.expect_err("a: drained");
	check_bucket_at(&table, "@b:x", limit(0.17, 1), now, CAP, Debit::Yes)
		.expect("guessing one account must not throttle another");
}

#[test]
fn zero_burst_or_zero_rate_disables() {
	let now = Instant::now();

	for disabled in [limit(0.0, 3), limit(0.17, 0), limit(-1.0, 3), limit(f64::NAN, 3)] {
		let table = Ratelimiter::default();
		for _ in 0..100 {
			check_bucket_at(&table, "@a:x", disabled, now, CAP, Debit::Yes)
				.expect("a disabled bucket never refuses");
		}

		assert!(table.lock().expect("locked").is_empty(), "a disabled bucket stores nothing");
	}
}

#[test]
fn full_table_prunes_refilled_buckets() {
	let table = Ratelimiter::default();
	let start = Instant::now();

	for i in 0..CAP {
		check_bucket_at(&table, &format!("@u{i}:x"), limit(1.0, 3), start, CAP, Debit::Yes)
			.expect("filling the table");
	}

	// One token was spent from three; at one per second, all are full again.
	check_bucket_at(&table, "@new:x", limit(1.0, 3), at(start, 2.0), CAP, Debit::Yes)
		.expect("a refilled bucket is pruned to admit a new account");

	let buckets = table.lock().expect("locked");
	assert!(buckets.len() <= CAP, "the table stays within its cap");
	assert!(buckets.contains_key("@new:x"), "the new account was admitted");
}

#[test]
fn full_table_never_evicts_a_limiting_bucket() {
	let table = Ratelimiter::default();
	let start = Instant::now();
	let slow = limit(0.003, 1);

	// The victim's bucket is drained and is the oldest in the table.
	check_bucket_at(&table, "@victim:x", slow, start, CAP, Debit::Yes).expect("victim's one try");

	for i in 1..CAP {
		check_bucket_at(&table, &format!("@spray{i}:x"), slow, at(start, 1.0), CAP, Debit::Yes)
			.expect("filling the table");
	}

	let refusal = check_bucket_at(&table, "@one-more:x", slow, at(start, 2.0), CAP, Debit::Yes)
		.expect_err("a full table of limiting buckets fails closed for a new account");
	assert!(
		stated_retry_after(&refusal).is_none(),
		"a full-table refusal states no delay it cannot know"
	);

	check_bucket_at(&table, "@victim:x", slow, at(start, 2.0), CAP, Debit::Yes)
		.expect_err("the spray must not have reset the victim's limit");
	assert!(
		table
			.lock()
			.expect("locked")
			.contains_key("@victim:x"),
		"the victim's bucket is still in the table"
	);
}

#[test]
fn login_gate_peeks_failed_then_debits_account() {
	let failed = Ratelimiter::default();
	let account = Ratelimiter::default();
	let now = Instant::now();
	let failed_limit = limit(0.17, 1);
	let account_limit = limit(0.003, 5);

	check_login_at(&failed, &account, "@a:x", failed_limit, account_limit, now, CAP)
		.expect("first attempt");
	assert!(failed.lock().expect("locked").is_empty(), "the gate only peeks the failed axis");
	assert!(
		(tokens(&account, "@a:x") - 4.0).abs() < 1e-9,
		"the gate debits the account axis"
	);

	// A recorded failure drains the failed axis, and the gate must then refuse
	// without spending the account axis on the refused attempt.
	check_bucket_at(&failed, "@a:x", failed_limit, now, CAP, Debit::Yes)
		.expect("failure recorded");
	check_login_at(&failed, &account, "@a:x", failed_limit, account_limit, now, CAP)
		.expect_err("a drained failed axis refuses");
	assert!(
		(tokens(&account, "@a:x") - 4.0).abs() < 1e-9,
		"a refusal on the failed axis does not debit the account axis"
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

	check_bucket_at(&table, "@a:x", limit(0.5, 1), now, CAP, Debit::Yes).expect("first");
	let error = check_bucket_at(&table, "@a:x", limit(0.5, 1), now, CAP, Debit::Yes)
		.expect_err("second is refused");

	assert_eq!(error.status_code(), http::StatusCode::TOO_MANY_REQUESTS);
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
