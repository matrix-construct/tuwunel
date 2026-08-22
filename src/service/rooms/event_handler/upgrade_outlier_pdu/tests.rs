use std::{io::Error as IoError, ops::Range, time::Duration};

use tuwunel_core::{Error, Result, err, utils::continue_exponential_backoff};

use super::{AuthCheckOutcome, UPGRADE_RETRY, current_state_auth_outcome};

#[tokio::test]
async fn current_state_auth_lookup_failure_propagates() {
	let auth_events: Result = Err(err!(Database("injected current-state lookup failure")));
	let outcome =
		current_state_auth_outcome(auth_events, async |()| Ok(AuthCheckOutcome::Allow)).await;

	assert!(matches!(outcome, Err(Error::Database(..))));
}

#[tokio::test]
async fn current_state_auth_evaluation_failure_propagates() {
	let outcome = current_state_auth_outcome(Ok(()), async |()| {
		Err(IoError::other("injected current-state auth failure").into())
	})
	.await;

	assert!(matches!(outcome, Err(Error::Io(..))));
}

#[test]
fn upgrade_retry_releases_after_the_window() {
	let Range { start, end } = UPGRADE_RETRY;

	assert!(continue_exponential_backoff(start, end, Duration::from_mins(4), 1));
	assert!(!continue_exponential_backoff(start, end, Duration::from_mins(6), 1));
}

#[test]
fn upgrade_retry_widens_but_stays_capped() {
	let Range { start, end } = UPGRADE_RETRY;

	assert!(continue_exponential_backoff(start, end, Duration::from_mins(6), 2));
	assert!(!continue_exponential_backoff(start, end, end, 1_000));
}
