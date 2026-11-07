//! Tests for the shared TURN credential cache.
//!
//! The coverage pins when cached credentials are served, renewed or held back
//! after a failure, and that neither credentials nor a failure carry over from
//! one provider to another.

use std::time::{Duration, Instant};

use super::{Cache, Credentials, Lookup, RETRY_DELAY};

const ORIGIN: &str = "https://relay.example/";

const OTHER: &str = "0123456789abcdef";

#[test]
fn served_until_half_life_then_due_until_expiry() {
	let now = Instant::now();
	let cache = Cache::fetched(ORIGIN, credentials(secs(100)), now);

	let young = cache.lookup(ORIGIN, now + secs(49));

	assert!(matches!(young, Lookup::Fresh(_)));
	assert_eq!(ttl(&young), Some(secs(51)), "the lifetime served is what is left of it");

	let old = cache.lookup(ORIGIN, now + secs(50));
	let remaining = cache.remaining(now + secs(50)).map(|c| c.ttl);

	assert!(matches!(old, Lookup::Due));
	assert_eq!(remaining, Some(secs(50)), "still served should the renewal fail");

	let expired = cache.lookup(ORIGIN, now + secs(100));

	assert!(matches!(expired, Lookup::Due));
	assert!(cache.remaining(now + secs(100)).is_none());
}

#[test]
fn failure_holds_fetches_back_for_the_retry_delay() {
	let fetched = Instant::now();
	let now = fetched + secs(600);
	let cache = Cache {
		failed: Some(now),
		..Cache::fetched(ORIGIN, credentials(secs(1000)), fetched)
	};

	let held = cache.lookup(ORIGIN, now + RETRY_DELAY.saturating_sub(secs(1)));

	assert!(matches!(held, Lookup::Held(Some(_))), "stale credentials are still served");

	let due = cache.lookup(ORIGIN, now + RETRY_DELAY);

	assert!(matches!(due, Lookup::Due));
}

#[test]
fn failure_under_another_provider_starts_afresh() {
	let now = Instant::now();
	let mut cache = Cache::fetched(ORIGIN, credentials(secs(100)), now);

	cache.record_failure(OTHER, now);

	assert!(matches!(cache.lookup(OTHER, now), Lookup::Held(None)));
	assert!(matches!(cache.lookup(ORIGIN, now), Lookup::Due), "nothing is held back");
}

#[test]
fn failure_under_the_same_provider_keeps_its_credentials() {
	let now = Instant::now();
	let mut cache = Cache::fetched(ORIGIN, credentials(secs(100)), now);

	cache.record_failure(ORIGIN, now + secs(60));

	assert!(matches!(cache.lookup(ORIGIN, now + secs(61)), Lookup::Held(Some(_))));
}

#[test]
fn credentials_expiring_before_a_failure_are_not_served() {
	let now = Instant::now();
	let mut cache = Cache::fetched(ORIGIN, credentials(secs(100)), now);

	cache.record_failure(ORIGIN, now + secs(100));

	assert!(cache.remaining(now + secs(100)).is_none());
	assert!(matches!(cache.lookup(ORIGIN, now + secs(100)), Lookup::Held(None)));
}

#[test]
fn another_provider_never_gets_cached_credentials() {
	let now = Instant::now();
	let cache = Cache::fetched(ORIGIN, credentials(secs(100)), now);

	assert!(matches!(cache.lookup(OTHER, now), Lookup::Due));
}

#[test]
fn empty_cache_is_due() {
	assert!(matches!(Cache::default().lookup(ORIGIN, Instant::now()), Lookup::Due));
}

fn credentials(ttl: Duration) -> Credentials {
	Credentials {
		username: "u".to_owned(),
		password: "p".to_owned(),
		uris: vec!["turn:relay.example".to_owned()],
		ttl,
	}
}

fn secs(secs: u64) -> Duration { Duration::from_secs(secs) }

fn ttl(lookup: &Lookup) -> Option<Duration> {
	match lookup {
		| Lookup::Fresh(credentials) => Some(credentials.ttl),
		| Lookup::Held(remaining) => remaining.as_ref().map(|c| c.ttl),
		| Lookup::Due => None,
	}
}
