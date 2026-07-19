use std::{
	sync::{Arc, RwLock},
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;

use super::{Get, MAX_HTTP_DATE_SECONDS, Put, Service};

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
fn delete_does_not_check_expiry() {
	let service = service();
	let (id, _) = service.create_at(Bytes::new(), time(60), Duration::ZERO, 100);

	assert!(service.delete(&id));
	assert!(!service.delete(&id));
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
		sessions: RwLock::default(),
		services: Arc::new(crate::services::OnceServices::default()),
	}
}

fn time(seconds: u64) -> SystemTime {
	UNIX_EPOCH
		.checked_add(Duration::from_secs(seconds))
		.expect("test time should fit in SystemTime")
}
