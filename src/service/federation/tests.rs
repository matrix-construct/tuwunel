#![allow(clippy::arithmetic_side_effects)]

use std::time::{Duration, UNIX_EPOCH};

use http::StatusCode;
use ruma::{OwnedServerName, api::error::ErrorBody};
use serde_json::Value;
use tuwunel_core::{Error, err};

use super::peer::{
	Backoff, Classification, MAX_BACKOFF, ShouldAttempt, attempt_verdict, classify,
	classify_error, failure_secs,
};

fn federation_error(status: StatusCode) -> Error {
	let server = OwnedServerName::try_from("remote.example").expect("valid server name");
	let body = ErrorBody::Json(Value::Null);

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
