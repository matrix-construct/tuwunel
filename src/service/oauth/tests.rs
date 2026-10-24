use std::{
	net::{IpAddr, Ipv4Addr},
	time::{Duration, Instant},
};

use super::{Ratelimiter, Session, check_bucket_at};

#[test]
fn rate_limiter_evicts_oldest_without_exceeding_cap() {
	let now = Instant::now();
	let oldest = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
	let retained = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2));
	let incoming = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 3));
	let table = Ratelimiter::new(
		[(oldest, (now, 1.5)), (retained, (now + Duration::from_millis(1), 1.5))].into(),
	);

	check_bucket_at(&table, incoming, 0.0, 2.0, now + Duration::from_millis(2), 2)
		.expect("new client should be accepted");

	let buckets = table
		.lock()
		.expect("rate limiter table lock should not be poisoned");

	assert_eq!(buckets.len(), 2, "rate-limit table must stay capped");
	assert!(!buckets.contains_key(&oldest), "oldest bucket should be evicted");
	assert!(buckets.contains_key(&retained), "newer bucket should be retained");
	assert!(buckets.contains_key(&incoming), "incoming bucket should be inserted");
	drop(buckets);

	check_bucket_at(&table, incoming, 0.0, 2.0, now + Duration::from_millis(3), 2)
		.expect("existing client should be accepted");

	let buckets = table
		.lock()
		.expect("rate limiter table lock should not be poisoned");

	assert_eq!(buckets.len(), 2, "existing key must not grow the table");
	assert!(buckets.contains_key(&retained), "existing hit must not evict another bucket");
}

#[test]
fn session_debug_masks_credentials() {
	let secrets = [
		"access_token",
		"id_token",
		"refresh_token",
		"code_verifier",
		"cookie_nonce",
		"query_nonce",
	];

	let sentinel = |field: &str| format!("{field}-sentinel");
	let [access_token, id_token, refresh_token, code_verifier, cookie_nonce, query_nonce] =
		secrets.map(sentinel).map(Some);

	let session = Session {
		sess_id: Some("visible-session".to_owned()),
		access_token,
		id_token,
		refresh_token,
		code_verifier,
		cookie_nonce,
		query_nonce,
		..Default::default()
	};

	let dump = format!("{session:#?}");

	assert!(dump.contains("visible-session"), "sess_id should stay visible: {dump}");

	for field in secrets {
		assert!(!dump.contains(&sentinel(field)), "{field} leaked in Debug: {dump}");
		assert!(
			dump.contains(&format!("{field}: Some(<redacted>)")),
			"{field} should appear as Some(<redacted>): {dump}"
		);
	}
}
