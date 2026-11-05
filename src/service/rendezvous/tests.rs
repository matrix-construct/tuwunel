//! Rendezvous session service tests.
//!
//! The cases exercise conditional updates, expiration, capacity eviction, and rate-limit behavior
//! with deterministic timestamps.

use std::{
	net::{IpAddr, Ipv4Addr},
	sync::{Arc, Mutex, RwLock},
	time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use ruma::TransactionId;

use super::{
	Attempt, Get, MAX_HTTP_DATE_SECONDS, MAX_TRANSACTIONS, Meta, Put, RATELIMIT_MAP_CAP,
	Ratelimiter, Service, SessionId, check_bucket_at,
};

struct Fixture {
	service: Service,
	now: SystemTime,
	id: SessionId,
	created: Meta,
}

const TTL: Duration = Duration::from_mins(3);

#[test]
fn create_and_conditional_get() {
	let Fixture { service, now, id, created } = fixture(10, b"");

	assert_eq!(id.len(), 32);
	assert_eq!(created.etag.len(), 45);
	assert!(created.etag.starts_with('"'));
	assert!(created.etag.ends_with('"'));
	assert_eq!(created.last_modified, now);
	assert_eq!(created.expires_at, now + TTL);
	assert_eq!(service.get_at(&id, None, now), Get::Data { data: Bytes::new(), meta: created },);
	assert_eq!(service.get_at(&id, Some(created.etag.as_str()), now), Get::NotModified(created),);
	assert_eq!(service.get_at(&id, Some("*"), now), Get::NotModified(created));

	let (_, other) = service.create_at(Bytes::new(), now, TTL, 100);

	assert_ne!(created.sequence_token(), other.sequence_token());
}

#[test]
fn matching_put_always_advances_etag() {
	let same = b"same";
	let Fixture { service, now, id, created } = fixture(20, same);
	let data = Bytes::from_static(same);
	let Put::Accepted(first) = service.put_at(&id, &created.etag, data.clone(), now, TTL) else {
		panic!("matching PUT should be accepted");
	};

	assert_ne!(first.etag, created.etag);
	assert_eq!(first.last_modified, now + Duration::from_millis(1));

	let Put::Accepted(second) = service.put_at(&id, &first.etag, data, now, TTL) else {
		panic!("second matching PUT should be accepted");
	};

	assert_ne!(second.etag, first.etag);
	assert_eq!(second.last_modified, now + Duration::from_millis(2));
}

#[test]
fn stale_put_distinguishes_retries_from_conflicts() {
	let Fixture { service, now, id, created } = fixture(30, b"first");
	let Put::Accepted(current) = service.put_at(
		&id,
		&created.etag,
		Bytes::from_static(b"current"),
		now + Duration::from_secs(1),
		TTL,
	) else {
		panic!("matching PUT should be accepted");
	};

	assert_eq!(
		service.put_at(
			&id,
			&created.etag,
			Bytes::from_static(b"different"),
			now + Duration::from_secs(2),
			TTL,
		),
		Put::PreconditionFailed(current),
	);

	let Put::Accepted(retry) = service.put_at(
		&id,
		&created.etag,
		Bytes::from_static(b"current"),
		now + Duration::from_secs(3),
		TTL,
	) else {
		panic!("idempotent retry should be accepted");
	};

	assert_eq!(retry.etag, current.etag);
	assert_eq!(retry.last_modified, current.last_modified);
	assert_eq!(retry.expires_at, now + Duration::from_secs(3) + TTL);
}

#[test]
fn sequence_token_put_uses_unquoted_validator() {
	let Fixture { service, now, id, created } = fixture(35, b"first");
	let stale = created.sequence_token();

	assert_eq!(stale.len(), 43);
	assert!(!stale.contains('"'));

	let at = now + Duration::from_secs(1);
	let current = sent(&service, &id, attempt("one", stale), b"current", at);
	let conflict = put(&service, &id, attempt("two", stale), b"different", at);

	assert_ne!(current.sequence_token(), stale);
	assert_eq!(conflict, Put::PreconditionFailed(current));
}

#[test]
fn stale_token_conflicts_even_with_identical_data() {
	let Fixture { service, now, id, created } = fixture(36, b"first");
	let stale = created.sequence_token();
	let same = b"current";
	let current = sent(&service, &id, attempt("one", stale), same, now);
	let conflict = put(&service, &id, attempt("two", stale), same, now);

	assert_eq!(conflict, Put::PreconditionFailed(current));
}

#[test]
fn recorded_transaction_replays_without_touching_the_session() {
	let Fixture { service, now, id, created } = fixture(37, b"first");
	let txn_id = "one";
	let first = sent(&service, &id, attempt(txn_id, created.sequence_token()), b"second", now);
	let second = sent(&service, &id, attempt("two", first.sequence_token()), b"third", now);
	let at = now + Duration::from_secs(1);
	let replay = put(&service, &id, attempt(txn_id, "stale"), b"ignored", at);
	let recorded = Meta { etag: first.etag, ..second };
	let current = Get::Data {
		data: Bytes::from_static(b"third"),
		meta: second,
	};

	assert_eq!(replay, Put::Accepted(recorded));
	assert_eq!(service.recorded_at(&id, txn(txn_id), at), Some(recorded));
	assert_eq!(service.get_at(&id, None, at), current);
}

#[test]
fn oldest_transaction_falls_out_at_the_cap() {
	let Fixture { service, now, id, created } = fixture(38, b"");
	let next = |meta: Meta, txn_id: String| {
		sent(&service, &id, attempt(&txn_id, meta.sequence_token()), b"", now)
	};

	let counts = 0..=MAX_TRANSACTIONS;
	let current = counts
		.map(|count| count.to_string())
		.fold(created, next);

	let retry = put(&service, &id, attempt("0", created.sequence_token()), b"", now);

	assert_eq!(service.recorded_at(&id, txn("0"), now), None);
	assert!(service.recorded_at(&id, txn("1"), now).is_some());
	assert_eq!(retry, Put::PreconditionFailed(current));
}

#[test]
fn rejected_send_records_nothing() {
	let Fixture { service, now, id, created } = fixture(39, b"");
	let rejected = put(&service, &id, attempt("one", "stale"), b"", now);

	assert_eq!(rejected, Put::PreconditionFailed(created));
	assert_eq!(service.recorded_at(&id, txn("one"), now), None);

	sent(&service, &id, attempt("one", created.sequence_token()), b"", now);
}

#[test]
fn transactions_stay_within_their_session() {
	let Fixture { service, now, id, created } = fixture(39, b"");
	let (other, _) = service.create_at(Bytes::new(), now, TTL, 100);

	sent(&service, &id, attempt("one", created.sequence_token()), b"", now);

	assert_eq!(service.recorded_at(&other, txn("one"), now), None);
}

#[test]
fn recorded_needs_a_live_session() {
	let Fixture { service, now, id, created } = fixture(39, b"");
	let token = created.sequence_token();
	let expires_at = sent(&service, &id, attempt("one", token), b"", now).expires_at;
	let last_moment = expires_at - Duration::from_millis(1);

	assert!(
		service
			.recorded_at(&id, txn("one"), last_moment)
			.is_some()
	);

	assert_eq!(service.recorded_at(&id, txn("one"), expires_at), None);

	let expired = put(&service, &id, attempt("one", token), b"", expires_at);

	assert_eq!(expired, Put::NotFound);
	assert_eq!(service.recorded_at(&id, txn("one"), now), None);
}

#[test]
fn relative_expiry_clamps_at_zero() {
	let Fixture { now, created, .. } = fixture(37, b"");

	assert_eq!(created.expires_in_at(now + Duration::from_mins(1)), Duration::from_mins(2));
	assert_eq!(created.expires_in_at(now + TTL + Duration::from_secs(1)), Duration::ZERO);
}

#[test]
fn rate_limiter_refills_and_stays_bounded() {
	let table = Ratelimiter::default();
	let now = Instant::now();
	let client = IpAddr::V4(Ipv4Addr::LOCALHOST);

	check_bucket_at(&table, client, 2.0, 2.0, now).expect("first request should be accepted");
	check_bucket_at(&table, client, 2.0, 2.0, now).expect("burst request should be accepted");
	assert!(check_bucket_at(&table, client, 2.0, 2.0, now).is_err());
	check_bucket_at(&table, client, 2.0, 2.0, now + Duration::from_millis(500))
		.expect("refilled request should be accepted");

	let end = u32::try_from(RATELIMIT_MAP_CAP).expect("rate-limit map cap fits in u32");

	for address in 0_u32..=end {
		let client = IpAddr::V4(Ipv4Addr::from(address));

		check_bucket_at(&table, client, 1.0, 1.0, now)
			.expect("new client request should be accepted");
	}

	assert_eq!(table.lock().expect("locked for reading").len(), RATELIMIT_MAP_CAP);
}

#[test]
fn expiry_is_lazy_and_put_refreshes_it() {
	let Fixture { service, now, id, created } = fixture(40, b"");
	let Put::Accepted(updated) = service.put_at(
		&id,
		&created.etag,
		Bytes::from_static(b"updated"),
		now + Duration::from_secs(1),
		TTL,
	) else {
		panic!("matching PUT should be accepted");
	};

	assert_eq!(updated.expires_at, now + Duration::from_secs(1) + TTL);
	assert!(matches!(
		service.get_at(&id, None, updated.expires_at - Duration::from_millis(1)),
		Get::Data { .. },
	));

	assert_eq!(service.get_at(&id, None, updated.expires_at), Get::NotFound);
	assert_eq!(service.put_at(&id, "*", Bytes::new(), updated.expires_at, TTL), Put::NotFound,);
}

#[test]
fn create_prunes_expired_then_evicts_oldest() {
	let service = service();
	let now = time(50);
	let (expired, _) = service.create_at(Bytes::new(), now, Duration::from_secs(1), 2);
	let (kept, _) = service.create_at(Bytes::new(), now + Duration::from_millis(1), TTL, 2);
	let (newest, _) = service.create_at(Bytes::new(), now + Duration::from_secs(2), TTL, 2);

	assert_eq!(service.get_at(&expired, None, now + Duration::from_secs(2)), Get::NotFound);
	assert!(matches!(
		service.get_at(&kept, None, now + Duration::from_secs(2)),
		Get::Data { .. },
	));

	assert!(matches!(
		service.get_at(&newest, None, now + Duration::from_secs(2)),
		Get::Data { .. },
	));

	let (replacement, _) = service.create_at(Bytes::new(), now + Duration::from_secs(3), TTL, 2);

	assert_eq!(service.get_at(&kept, None, now + Duration::from_secs(3)), Get::NotFound);
	assert!(matches!(
		service.get_at(&replacement, None, now + Duration::from_secs(3)),
		Get::Data { .. },
	));
}

#[test]
fn delete_does_not_check_expiry() {
	let service = service();
	let (id, _) = service.create_at(Bytes::new(), time(60), Duration::ZERO, 100);

	assert!(service.delete(&id));
	assert!(!service.delete(&id));
}

#[test]
fn active_delete_rejects_expired_sessions() {
	let service = service();
	let now = time(60);
	let (expired, _) = service.create_at(Bytes::new(), now, Duration::ZERO, 100);
	let (active, _) = service.create_at(Bytes::new(), now, TTL, 100);

	assert!(!service.delete_if_active_at(&expired, now));
	assert!(service.delete_if_active_at(&active, now));
	assert!(!service.delete_if_active_at(&active, now));
}

#[test]
fn zero_capacity_retains_one_session() {
	let service = service();
	let now = time(70);
	let (first, _) = service.create_at(Bytes::new(), now, TTL, 0);
	let (second, _) = service.create_at(Bytes::new(), now, TTL, 0);

	assert_eq!(service.get_at(&first, None, now), Get::NotFound);
	assert!(matches!(service.get_at(&second, None, now), Get::Data { .. }));
}

#[test]
fn huge_ttl_caps_at_latest_http_date() {
	let service = service();
	let (_, meta) = service.create_at(Bytes::new(), time(10), Duration::MAX, 100);

	assert_eq!(meta.expires_at, time(MAX_HTTP_DATE_SECONDS));
}

fn fixture(seconds: u64, data: &'static [u8]) -> Fixture {
	let service = service();
	let now = time(seconds);
	let (id, created) = service.create_at(Bytes::from_static(data), now, TTL, 100);

	Fixture { service, now, id, created }
}

fn service() -> Service {
	Service {
		sessions: RwLock::default(),
		ratelimiter: Mutex::default(),
		services: Arc::new(crate::services::OnceServices::default()),
	}
}

fn time(seconds: u64) -> SystemTime {
	UNIX_EPOCH
		.checked_add(Duration::from_secs(seconds))
		.expect("test time should fit in SystemTime")
}

#[track_caller]
fn sent(
	service: &Service,
	id: &str,
	attempt: Attempt<'_>,
	data: &'static [u8],
	now: SystemTime,
) -> Meta {
	let Put::Accepted(meta) = put(service, id, attempt, data, now) else {
		panic!("send should be accepted");
	};

	meta
}

fn put(
	service: &Service,
	id: &str,
	attempt: Attempt<'_>,
	data: &'static [u8],
	now: SystemTime,
) -> Put {
	service.put_token_at(id, attempt, Bytes::from_static(data), now, TTL)
}

fn attempt<'a>(txn_id: &'a str, sequence_token: &'a str) -> Attempt<'a> {
	Attempt { txn_id: txn(txn_id), sequence_token }
}

fn txn(txn_id: &str) -> &TransactionId { txn_id.into() }
