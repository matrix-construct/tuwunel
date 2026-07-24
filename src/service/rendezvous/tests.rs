use std::{
	net::{IpAddr, Ipv4Addr},
	sync::{Arc, Mutex, RwLock},
	time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;

use super::{
	Get, MAX_HTTP_DATE_SECONDS, Put, RATELIMIT_MAP_CAP, Ratelimiter, Service, check_bucket_at,
};

const TTL: Duration = Duration::from_mins(3);

#[test]
fn create_and_conditional_get() {
	let service = service();
	let now = time(10);
	let (id, meta) = service.create_at(Bytes::new(), now, TTL, 100);

	assert_eq!(id.len(), 32);
	assert_eq!(meta.etag.len(), 45);
	assert!(meta.etag.starts_with('"'));
	assert!(meta.etag.ends_with('"'));
	assert_eq!(meta.last_modified, now);
	assert_eq!(meta.expires_at, now + TTL);
	assert_eq!(service.get_at(&id, None, now), Get::Data { data: Bytes::new(), meta },);
	assert_eq!(service.get_at(&id, Some(meta.etag.as_str()), now), Get::NotModified(meta),);
	assert_eq!(service.get_at(&id, Some("*"), now), Get::NotModified(meta));

	let (_, other) = service.create_at(Bytes::new(), now, TTL, 100);
	assert_ne!(meta.sequence_token(), other.sequence_token());
}

#[test]
fn matching_put_always_advances_etag() {
	let service = service();
	let now = time(20);
	let data = Bytes::from_static(b"same");
	let (id, created) = service.create_at(data.clone(), now, TTL, 100);
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
	let service = service();
	let now = time(30);
	let (id, created) = service.create_at(Bytes::from_static(b"first"), now, TTL, 100);
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
	let service = service();
	let now = time(35);
	let (id, created) = service.create_msc4388_at(Bytes::from_static(b"first"), now, TTL, 100);

	assert_eq!(created.sequence_token().len(), 43);
	assert!(!created.sequence_token().contains('"'));

	let Put::Accepted(current) = service.put_token_at(
		&id,
		created.sequence_token(),
		Bytes::from_static(b"current"),
		now + Duration::from_secs(1),
		TTL,
	) else {
		panic!("matching sequence token should be accepted");
	};

	assert_ne!(current.sequence_token(), created.sequence_token());
	assert_eq!(
		service.put_token_at(
			&id,
			created.sequence_token(),
			Bytes::from_static(b"different"),
			now + Duration::from_secs(2),
			TTL,
		),
		Put::PreconditionFailed(current),
	);

	let Put::Accepted(retry) = service.put_token_at(
		&id,
		created.sequence_token(),
		Bytes::from_static(b"current"),
		now + Duration::from_secs(3),
		TTL,
	) else {
		panic!("idempotent token retry should be accepted");
	};

	assert_eq!(retry.sequence_token(), current.sequence_token());
	assert_eq!(retry.last_modified, current.last_modified);
}

#[test]
fn relative_expiry_clamps_at_zero() {
	let service = service();
	let now = time(37);
	let (_, meta) = service.create_at(Bytes::new(), now, TTL, 100);

	assert_eq!(meta.expires_in_at(now + Duration::from_mins(1)), Duration::from_mins(2));
	assert_eq!(meta.expires_in_at(now + TTL + Duration::from_secs(1)), Duration::ZERO);
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
	let service = service();
	let now = time(40);
	let (id, created) = service.create_at(Bytes::new(), now, TTL, 100);
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
fn transports_isolate_capacity_and_ids() {
	let service = service();
	let now = time(55);
	let msc4388_data = Bytes::from_static(b"msc4388");
	let msc4108_data = Bytes::from_static(b"msc4108");
	let (msc4388_id, msc4388_meta) = service.create_msc4388_at(msc4388_data.clone(), now, TTL, 1);
	let (evicted_msc4108_id, _) =
		service.create_at(Bytes::new(), now + Duration::from_millis(1), TTL, 1);

	let (msc4108_id, msc4108_meta) =
		service.create_at(msc4108_data.clone(), now + Duration::from_millis(2), TTL, 1);

	let checked_at = now + Duration::from_millis(2);

	assert_eq!(service.get_at(&evicted_msc4108_id, None, checked_at), Get::NotFound);
	assert_eq!(service.get_at(&msc4388_id, None, checked_at), Get::NotFound);
	assert_eq!(service.get_msc4388_at(&msc4108_id, checked_at), Get::NotFound);
	assert_eq!(service.put_at(&msc4388_id, "*", Bytes::new(), checked_at, TTL), Put::NotFound,);
	assert!(!service.delete(&msc4388_id));
	assert_eq!(
		service.put_token_at(
			&msc4108_id,
			msc4108_meta.sequence_token(),
			Bytes::new(),
			checked_at,
			TTL,
		),
		Put::NotFound,
	);

	assert!(!service.delete_if_active_at(&msc4108_id, checked_at));

	let expected_msc4388 = Get::Data { data: msc4388_data, meta: msc4388_meta };

	let expected_msc4108 = Get::Data { data: msc4108_data, meta: msc4108_meta };

	assert_eq!(service.get_msc4388_at(&msc4388_id, checked_at), expected_msc4388);
	assert_eq!(service.get_at(&msc4108_id, None, checked_at), expected_msc4108);
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
	let (expired, _) = service.create_msc4388_at(Bytes::new(), now, Duration::ZERO, 100);
	let (active, _) = service.create_msc4388_at(Bytes::new(), now, TTL, 100);

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

fn service() -> Service {
	Service {
		msc4108_sessions: RwLock::default(),
		msc4388_sessions: RwLock::default(),
		ratelimiter: Mutex::default(),
		services: Arc::new(crate::services::OnceServices::default()),
	}
}

fn time(seconds: u64) -> SystemTime {
	UNIX_EPOCH
		.checked_add(Duration::from_secs(seconds))
		.expect("test time should fit in SystemTime")
}
