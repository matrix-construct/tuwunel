//! Tests for the threepid token-bucket capacity policy.
//!
//! The coverage verifies that insertion beyond the fixed map capacity evicts
//! an old bucket. It also confirms that the table does not grow past its bound.

use std::time::{Duration, Instant};

use super::{EmailKey, Ratelimiter, check_bucket_at};

#[test]
fn rate_limiter_evicts_oldest_without_exceeding_cap() {
	let now = Instant::now();
	let table: Ratelimiter<EmailKey> = Ratelimiter::new(
		[
			("oldest".into(), (now, 1.5)),
			("retained".into(), (now + Duration::from_millis(1), 1.5)),
		]
		.into(),
	);

	check_bucket_at(
		&table,
		"incoming",
		|| "incoming".into(),
		0.0,
		2.0,
		now + Duration::from_millis(2),
		2,
	)
	.expect("new key should be accepted");

	let buckets = table.lock().expect("locked for inspection");

	assert_eq!(buckets.len(), 2, "rate-limit table must stay capped");
	assert!(!buckets.contains_key("oldest"), "oldest bucket should be evicted");
	assert!(buckets.contains_key("retained"), "newer bucket should be retained");
	assert!(buckets.contains_key("incoming"), "incoming bucket should be inserted");
	drop(buckets);

	check_bucket_at(
		&table,
		"incoming",
		|| panic!("existing key lookup constructed an owned key"),
		0.0,
		2.0,
		now + Duration::from_millis(3),
		2,
	)
	.expect("existing key should be accepted");

	let buckets = table.lock().expect("locked for inspection");

	assert_eq!(buckets.len(), 2, "existing key must not grow the table");
	assert!(buckets.contains_key("retained"), "existing hit must not evict another bucket");
}
