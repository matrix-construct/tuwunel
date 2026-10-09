#![allow(clippy::arithmetic_side_effects)]

//! Tests peer failure classification, persistence, and retry backoff.
//!
//! The cases cover legacy and timestamped rows, class selection, grace tiers,
//! delay curves, deadlines, and saturation.

use std::{
	sync::Arc,
	time::{Duration, UNIX_EPOCH},
};

use bytes::Bytes;
use http::StatusCode;
use reqwest::{Request, Url};
use ruma::{
	OwnedEventId, OwnedServerName,
	api::{
		error::ErrorBody,
		federation::{
			discovery::get_server_version::v1::Request as VersionRequest,
			event::get_event::v1::Request as EventRequest,
		},
	},
};
use serde_json::Value;
use tuwunel_core::{Error, Result, config::Figment, err};

use super::peer::{
	Backoff, Classification, MAX_BACKOFF, ShouldAttempt, attempt_verdict, classify,
	classify_error, failure_secs, fold_streak, is_content_rejection,
};
use crate::{
	resolver::{cache::CachedDest, fed::FedDest},
	test_utils::fixture,
};

#[tokio::test]
async fn final_url_guard_checks_typed_ipv4_and_ipv6_literals() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	for url in [
		"https://0.0.0.0:8448/_matrix/federation/v1/version",
		"https://[::]:8448/_matrix/federation/v1/version",
		"https://[::ffff:127.0.0.1]:8448/_matrix/federation/v1/version",
	] {
		let url = Url::parse(url).expect("test URL parses");
		let error = fixture
			.services
			.federation
			.validate_url(&url)
			.expect_err("default-denied literal is rejected");

		assert!(
			error
				.to_string()
				.contains("Not allowed to send requests to this IP")
		);
	}

	for url in [
		"https://8.8.8.8:8448/_matrix/federation/v1/version",
		"https://[2001:4860:4860::8888]:8448/_matrix/federation/v1/version",
		"https://remote.example:8448/_matrix/federation/v1/version",
	] {
		let url = Url::parse(url).expect("test URL parses");

		fixture.services.federation.validate_url(&url)?;
	}

	Ok(())
}

#[tokio::test]
async fn cached_literal_routes_are_checked_during_request_preparation() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let logical = OwnedServerName::try_from("logical.example").expect("test server name parses");

	for socket in ["0.0.0.0:8448", "[::]:8448", "[::ffff:127.0.0.1]:8448"] {
		let cached = CachedDest {
			dest: FedDest::Literal(socket.parse().expect("test socket parses")),
			host: logical.as_str().into(),
			expire: CachedDest::default_expire(),
			srv: false,
		};

		fixture
			.services
			.resolver
			.cache
			.set_destination(&logical, &cached);

		let actual = fixture
			.services
			.resolver
			.get_actual_dest(&logical)
			.await?;

		let error = fixture
			.services
			.federation
			.prepare(&actual, &logical, VersionRequest {})
			.expect_err("cached denied literal is rejected before send");

		assert!(
			error
				.to_string()
				.contains("Not allowed to send requests to this IP")
		);
	}

	let socket = "[2001:4860:4860::8888]:9448"
		.parse()
		.expect("test socket parses");

	let cached = CachedDest {
		dest: FedDest::Literal(socket),
		host: logical.as_str().into(),
		expire: CachedDest::default_expire(),
		srv: false,
	};

	fixture
		.services
		.resolver
		.cache
		.set_destination(&logical, &cached);

	let actual = fixture
		.services
		.resolver
		.get_actual_dest(&logical)
		.await?;

	let event_id =
		OwnedEventId::try_from("$event:logical.example").expect("test event ID parses");

	let request = fixture
		.services
		.federation
		.prepare(&actual, &logical, EventRequest { event_id })?;

	assert_eq!(request.url().host_str(), Some("[2001:4860:4860::8888]"));
	assert_eq!(request.url().port(), Some(9448));
	let authorization = request_authorization(&request);

	assert!(authorization.contains(logical.as_str()), "{authorization}");

	Ok(())
}

fn request_authorization(request: &Request) -> &str {
	request
		.headers()
		.get(http::header::AUTHORIZATION)
		.and_then(|value| value.to_str().ok())
		.expect("signed federation request has an authorization header")
}

#[tokio::test]
async fn configured_ipv6_mapped_range_remains_load_bearing() -> Result {
	let config = Figment::new().merge(("ip_range_denylist", ["::ffff:0:0/96"]));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let mapped = Url::parse("https://[::ffff:8.8.8.8]:8448/").expect("test URL parses");
	let native = Url::parse("https://[2001:4860:4860::8888]:8448/").expect("test URL parses");
	let ipv4 = Url::parse("https://8.8.8.8:8448/").expect("test URL parses");

	assert!(
		fixture
			.services
			.federation
			.validate_url(&mapped)
			.is_err()
	);

	fixture
		.services
		.federation
		.validate_url(&native)?;

	fixture.services.federation.validate_url(&ipv4)?;

	Ok(())
}

#[tokio::test]
async fn configured_ipv4_range_checks_mapped_addresses_only() -> Result {
	let config = Figment::new().merge(("ip_range_denylist", ["127.0.0.0/8"]));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let mapped = Url::parse("https://[::ffff:127.0.0.1]:8448/").expect("test URL parses");
	let native = Url::parse("https://[::1]:8448/").expect("test URL parses");

	assert!(
		fixture
			.services
			.federation
			.validate_url(&mapped)
			.is_err()
	);

	fixture
		.services
		.federation
		.validate_url(&native)?;

	Ok(())
}

#[tokio::test]
async fn direct_and_delegated_routes_prepare_exact_authorities() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let event_id =
		OwnedEventId::try_from("$event:logical.example").expect("test event ID parses");

	let direct =
		OwnedServerName::try_from("[2001:4860:4860::8888]").expect("test server name parses");

	let actual = fixture
		.services
		.resolver
		.get_actual_dest(&direct)
		.await?;

	let request = fixture
		.services
		.federation
		.prepare(&actual, &direct, EventRequest { event_id: event_id.clone() })?;

	assert_eq!(request.url().host_str(), Some("[2001:4860:4860::8888]"));
	assert_eq!(request.url().port(), Some(8448));
	let authorization = request_authorization(&request);

	assert!(authorization.contains("destination=\"[2001:4860:4860::8888]\""));
	for (name, host, port, signed_destination) in [
		("8.8.8.8", "8.8.8.8", 8448, "destination=8.8.8.8"),
		("8.8.8.8:9448", "8.8.8.8", 9448, "destination=\"8.8.8.8:9448\""),
		("[::ffff:8.8.8.8]", "[::ffff:808:808]", 8448, "destination=\"[::ffff:8.8.8.8]\""),
		(
			"[::ffff:8.8.8.8]:9448",
			"[::ffff:808:808]",
			9448,
			"destination=\"[::ffff:8.8.8.8]:9448\"",
		),
		(
			"[2001:4860:4860::8888]:9448",
			"[2001:4860:4860::8888]",
			9448,
			"destination=\"[2001:4860:4860::8888]:9448\"",
		),
	] {
		let direct = OwnedServerName::try_from(name).expect("test server name parses");
		let actual = fixture
			.services
			.resolver
			.get_actual_dest(&direct)
			.await?;

		let request = fixture
			.services
			.federation
			.prepare(&actual, &direct, EventRequest { event_id: event_id.clone() })?;

		assert_eq!(request.url().host_str(), Some(host));
		assert_eq!(request.url().port(), Some(port));
		let authorization = request_authorization(&request);

		assert!(authorization.contains(signed_destination));
	}

	let logical = OwnedServerName::try_from("logical.example").expect("test server name parses");

	let denied = fixture
		.services
		.resolver
		.test_delegated_dest("[::1]")
		.await?;

	let error = fixture
		.services
		.federation
		.prepare(&denied, &logical, EventRequest { event_id: event_id.clone() })
		.expect_err("delegated denied literal is rejected before send");

	assert!(
		error
			.to_string()
			.contains("Not allowed to send requests to this IP")
	);

	let delegated = fixture
		.services
		.resolver
		.test_delegated_dest("[2001:4860:4860:0:0:0:0:8888]:9448")
		.await?;

	let request = fixture
		.services
		.federation
		.prepare(&delegated, &logical, EventRequest { event_id })?;

	assert_eq!(request.url().host_str(), Some("[2001:4860:4860::8888]"));
	assert_eq!(request.url().port(), Some(9448));
	let authorization = request_authorization(&request);

	assert!(authorization.contains("destination=logical.example"));

	Ok(())
}

#[tokio::test]
async fn empty_denylist_allows_final_private_literal_preparation() -> Result {
	let config = Figment::new().merge(("ip_range_denylist", Vec::<String>::new()));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let logical = OwnedServerName::try_from("logical.example").expect("test server name parses");
	let direct = OwnedServerName::try_from("[::1]").expect("test server name parses");
	let event_id =
		OwnedEventId::try_from("$event:logical.example").expect("test event ID parses");

	let actual = fixture
		.services
		.resolver
		.get_actual_dest(&direct)
		.await?;

	let request = fixture
		.services
		.federation
		.prepare(&actual, &logical, EventRequest { event_id })?;

	assert_eq!(request.url().host_str(), Some("[::1]"));
	assert_eq!(request.url().port(), Some(8448));

	Ok(())
}

#[tokio::test]
async fn configured_proxy_keeps_literal_destination_policy() -> Result {
	let proxy = serde_json::json!({
		"global": { "url": "socks5h://proxy.internal:1080" },
	});

	let config = Figment::new().merge(("proxy", proxy));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let denied = OwnedServerName::try_from("[::1]").expect("test server name parses");
	let error = fixture
		.services
		.resolver
		.get_actual_dest(&denied)
		.await
		.expect_err("proxied denied literal is rejected before preparation");

	assert!(
		error
			.to_string()
			.contains("Not allowed to send requests to this IP")
	);

	let allowed =
		OwnedServerName::try_from("[2001:4860:4860::8888]").expect("test server name parses");

	let event_id =
		OwnedEventId::try_from("$event:logical.example").expect("test event ID parses");

	let actual = fixture
		.services
		.resolver
		.get_actual_dest(&allowed)
		.await?;

	let request = fixture
		.services
		.federation
		.prepare(&actual, &allowed, EventRequest { event_id })?;

	assert_eq!(request.url().host_str(), Some("[2001:4860:4860::8888]"));
	assert_eq!(request.url().port(), Some(8448));

	Ok(())
}

fn federation_error(status: StatusCode) -> Error {
	let server = OwnedServerName::try_from("remote.example").expect("valid server name");
	let body = ErrorBody::Json(Value::Null);

	Error::Federation(server, body.into_error(status))
}

fn federation_error_notjson(status: StatusCode) -> Error {
	let server = OwnedServerName::try_from("remote.example").expect("valid server name");
	let deserialization_error =
		serde_json::from_slice::<Value>(b"<html>bad gateway</html>").expect_err("not valid json");

	let body = ErrorBody::NotJson {
		bytes: Bytes::from_static(b"<html>bad gateway</html>"),
		deserialization_error: Arc::new(deserialization_error),
	};

	Error::Federation(server, body.into_error(status))
}

#[test]
fn content_4xx_is_not_a_peer_failure() {
	for status in [
		StatusCode::BAD_REQUEST,
		StatusCode::UNAUTHORIZED,
		StatusCode::FORBIDDEN,
		StatusCode::NOT_FOUND,
	] {
		assert!(classify_error(&federation_error(status)).is_none(), "{status} recorded");
	}
}

#[test]
fn content_4xx_notjson_is_transient() {
	// A non-JSON 4xx body means a proxy or CDN answered, not the homeserver,
	// so it records a transient failure, unlike a content-level 4xx with JSON.
	for status in [
		StatusCode::BAD_REQUEST,
		StatusCode::UNAUTHORIZED,
		StatusCode::FORBIDDEN,
		StatusCode::NOT_FOUND,
	] {
		let verdict = classify_error(&federation_error_notjson(status));

		assert!(matches!(verdict, Some(Classification::Transient)), "{status} not transient");
	}
}

#[test]
fn gone_is_permanent() {
	let verdict = classify_error(&federation_error(StatusCode::GONE));

	assert!(matches!(verdict, Some(Classification::Permanent)));
}

#[test]
fn server_error_and_rate_limit_are_transient() {
	for status in [
		StatusCode::TOO_MANY_REQUESTS,
		StatusCode::INTERNAL_SERVER_ERROR,
		StatusCode::SERVICE_UNAVAILABLE,
	] {
		assert!(
			matches!(classify_error(&federation_error(status)), Some(Classification::Transient)),
			"{status} not transient"
		);
	}
}

#[test]
fn non_federation_error_is_transient() {
	let error = err!(BadServerResponse("transport failure"));

	assert!(matches!(classify_error(&error), Some(Classification::Transient)));
}

#[test]
fn content_rejection_is_only_the_unrecorded_class() {
	assert!(is_content_rejection(&federation_error(StatusCode::FORBIDDEN)));
	assert!(is_content_rejection(&federation_error(StatusCode::PAYLOAD_TOO_LARGE)));
	assert!(!is_content_rejection(&federation_error(StatusCode::GONE)));
	assert!(!is_content_rejection(&federation_error(StatusCode::BAD_GATEWAY)));
	assert!(!is_content_rejection(&federation_error(StatusCode::TOO_MANY_REQUESTS)));
	assert!(!is_content_rejection(&federation_error_notjson(StatusCode::NOT_FOUND)));
	assert!(!is_content_rejection(&err!(Request(Forbidden("local")))));
}

#[test]
fn legacy_row_has_no_timestamp() {
	let row = [u8::from(Classification::Permanent)];

	assert!(matches!(classify(&row), Classification::Permanent));
	assert_eq!(failure_secs(&row), None);
}

#[test]
fn timestamped_row_round_trips() {
	let secs: u64 = 1_700_000_000;
	let mut row = [0_u8; 9];
	row[0] = u8::from(Classification::Transient);
	row[1..].copy_from_slice(&secs.to_be_bytes());

	assert!(matches!(classify(&row), Classification::Transient));
	assert_eq!(failure_secs(&row), Some(secs));
}

fn transient_verdict(anchor_secs: u64, streak: u32, now: u64) -> ShouldAttempt {
	attempt_verdict(&Backoff {
		class: Classification::Transient,
		anchor_secs,
		streak,
		now,
		window_secs: 180,
		grace_secs: 15,
	})
}

fn no_before(secs: u64) -> ShouldAttempt {
	ShouldAttempt::No {
		earliest_retry: UNIX_EPOCH + Duration::from_secs(secs),
	}
}

#[test]
fn grace_tier_holds_then_releases() {
	// A lone transient failure retries once `grace` (15s) elapses.
	assert_eq!(transient_verdict(1000, 1, 1010), no_before(1015));
	assert!(matches!(transient_verdict(1000, 1, 1015), ShouldAttempt::Yes));
	assert!(matches!(transient_verdict(1000, 1, 2000), ShouldAttempt::Yes));
}

#[test]
fn quadratic_curve_climbs_with_streak() {
	// window * streak^2 past the anchor: streak 2 -> 720s, streak 3 -> 1620s.
	assert_eq!(transient_verdict(1000, 2, 1500), no_before(1720));
	assert_eq!(transient_verdict(1000, 3, 1500), no_before(2620));
}

#[test]
fn verdict_is_monotonic_and_honors_the_deadline() {
	// streak 2 anchors earliest_retry at 1000 + 720: No until exactly then,
	// then Yes and staying, never releasing early at a window boundary.
	for now in [1000, 1500, 1719] {
		assert_eq!(transient_verdict(1000, 2, now), no_before(1720), "released early at {now}");
	}

	for now in [1720, 1721, 100_000] {
		assert!(
			matches!(transient_verdict(1000, 2, now), ShouldAttempt::Yes),
			"not released at {now}"
		);
	}
}

#[test]
fn permanent_ignores_streak_and_caps_at_max() {
	let max = MAX_BACKOFF.as_secs();
	let permanent = |now| {
		attempt_verdict(&Backoff {
			class: Classification::Permanent,
			anchor_secs: 1000,
			streak: 1,
			now,
			window_secs: 180,
			grace_secs: 15,
		})
	};

	assert_eq!(permanent(1000 + max - 1), no_before(1000 + max));
	assert!(matches!(permanent(1000 + max), ShouldAttempt::Yes));
}

#[test]
fn curve_saturates_at_max_backoff() {
	// A large streak saturates window * streak^2 at MAX_BACKOFF, no overflow.
	assert_eq!(transient_verdict(1000, u32::MAX, 1000), no_before(1000 + MAX_BACKOFF.as_secs()));
}

#[test]
fn disabled_grace_uses_the_curve_from_the_first_failure() {
	let verdict = attempt_verdict(&Backoff {
		class: Classification::Transient,
		anchor_secs: 1000,
		streak: 1,
		now: 1000,
		window_secs: 180,
		grace_secs: 0,
	});

	// streak 1 with grace disabled uses window * 1 = 180s, not the grace tier.
	assert_eq!(verdict, no_before(1180));
}

#[test]
fn delay_secs_selects_the_tier() {
	let delay = |class, streak, grace_secs| {
		Backoff {
			class,
			anchor_secs: 0,
			streak,
			now: 0,
			window_secs: 180,
			grace_secs,
		}
		.delay_secs()
	};

	assert_eq!(delay(Classification::Permanent, 1, 15), MAX_BACKOFF.as_secs());
	assert_eq!(delay(Classification::Transient, 1, 15), 15);
	assert_eq!(delay(Classification::Transient, 3, 15), 1_620);
	assert_eq!(delay(Classification::Transient, u32::MAX, 15), MAX_BACKOFF.as_secs());
}

#[test]
fn fold_streak_tracks_anchor_and_oldest() {
	let window_secs = 180;

	// A legacy one-byte row carries no instant; the anchor falls back to the
	// bucket start.
	let legacy = [u8::from(Classification::Transient)];
	let first = fold_streak(window_secs, None, 10, &legacy);

	assert_eq!(first.anchor_secs, 10 * window_secs);
	assert_eq!(first.oldest_bucket, 10);
	assert_eq!(first.latest_bucket, 10);
	assert!(matches!(first.class, Classification::Transient));

	// A newer nine-byte row overrides the class and anchor, but the oldest
	// bucket sticks.
	let secs: u64 = 1_700_000_000;
	let mut newer = [0_u8; 9];
	newer[0] = u8::from(Classification::Permanent);
	newer[1..].copy_from_slice(&secs.to_be_bytes());

	let second = fold_streak(window_secs, Some(first), 12, &newer);

	assert_eq!(second.anchor_secs, secs);
	assert_eq!(second.oldest_bucket, 10);
	assert_eq!(second.latest_bucket, 12);
	assert!(matches!(second.class, Classification::Permanent));
}
