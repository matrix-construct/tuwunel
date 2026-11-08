use std::time::Duration;

use ruma::api::error::ErrorKind;
use tuwunel_service::delayed_events::Limits;

use super::{check_delay, delay_query::parse_delay};

const LIMITS: Limits = Limits { max_delay_ms: 1_000, max_scheduled: 1 };

#[test]
fn absent_delay_sends_at_once() {
	assert_eq!(parse_delay("").expect("empty query"), None);
	assert_eq!(parse_delay("user_id=%40bot%3Aexample.com&ts=1").expect("other params"), None);
}

#[test]
fn delay_reads_milliseconds_among_other_params() {
	let query = "ts=1&org.matrix.msc4140.delay=900&user_id=%40bot%3Aexample.com";

	assert_eq!(parse_delay(query).expect("valid delay"), Some(900));
}

#[test]
fn first_delay_wins_when_repeated() {
	let query = "org.matrix.msc4140.delay=5&org.matrix.msc4140.delay=7";

	assert_eq!(parse_delay(query).expect("valid delay"), Some(5));
}

#[test]
fn bare_or_invalid_delay_is_refused() {
	for query in [
		"org.matrix.msc4140.delay",
		"org.matrix.msc4140.delay=",
		"org.matrix.msc4140.delay=-1",
		"org.matrix.msc4140.delay=1.5",
		"org.matrix.msc4140.delay=soon",
	] {
		let error = parse_delay(query).expect_err(query);

		assert_eq!(error.kind(), ErrorKind::InvalidParam, "{query}");
	}
}

#[test]
fn delay_within_limit_passes() {
	assert_eq!(check_delay(Duration::from_millis(1), LIMITS, true).expect("min"), 1);
	assert_eq!(check_delay(Duration::from_millis(1_000), LIMITS, false).expect("max"), 1_000);
}

#[test]
fn zero_delay_is_refused() {
	let error = check_delay(Duration::ZERO, LIMITS, true).expect_err("zero delay");

	assert_eq!(error.kind(), ErrorKind::InvalidParam);
}

#[test]
fn delay_over_limit_uses_the_path_dialect() {
	let stable = check_delay(Duration::from_millis(1_001), LIMITS, true).expect_err("stable");
	let unstable =
		check_delay(Duration::from_millis(1_001), LIMITS, false).expect_err("unstable");

	assert_eq!(stable.kind(), ErrorKind::DelayTooLarge);
	assert_eq!(unstable.kind(), ErrorKind::UnstableDelayTooLarge);
}
