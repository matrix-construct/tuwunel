mod liveness;
mod pending;
mod performance;
mod refusal;

use std::{cmp::Reverse, iter::once, time::Duration};

use http::StatusCode;
use ruma::{OwnedServerName, api::error::ErrorBody};
use serde_json::Value;
use tokio::time::Instant;
use tuwunel_core::{Error, Result, config::Figment};

use super::{
	SendingFutures, TransactionStatus, TransactionStatuses, WakeQueue, completion, enqueue,
	fixture::fixture, pdu_id,
};
use crate::{
	federation::ShouldAttempt,
	sending::{Destination, SendingEvent},
	test_utils::fixture as service_fixture,
};

#[tokio::test]
async fn content_rejection_arms_the_sender_curve() -> Result {
	let Some(fixture) = fixture(false, -1).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let server: OwnedServerName = "rejected.example".try_into()?;
	let dest = Destination::Federation(server.clone());
	let mut futures = SendingFutures::new(); // response and wake state out-param
	let mut statuses = [(dest.clone(), TransactionStatus::Running { tries: 1 })].into();
	let mut wakes = WakeQueue::new(); // response and wake state out-param
	let started = Instant::now();
	let rejection = ErrorBody::Json(Value::Null).into_error(StatusCode::FORBIDDEN);
	let response = completion(Err((dest.clone(), Error::Federation(server.clone(), rejection))));

	sending
		.handle_response(response, &mut futures, &mut statuses, &mut wakes)
		.await;

	let verdict = fixture
		.services
		.federation
		.should_attempt(&server)
		.await;

	assert!(matches!(verdict, ShouldAttempt::Yes));

	assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Retrying { tries: 2 })));
	assert_eq!(wakes.len(), 1);

	let Reverse((due, armed)) = wakes.peek().expect("failure retry");
	let delay = due.duration_since(started);

	assert_eq!(armed, &dest);
	assert!(delay >= Duration::from_secs(4 * sending.server.config.sender_timeout));
	assert!(delay < Duration::from_secs(8 * sending.server.config.sender_timeout + 1));

	statuses.insert(dest.clone(), TransactionStatus::Running { tries: 2 });
	wakes.clear();
	wakes.push(Reverse((Instant::now(), dest)));
	sending
		.drain_due_wakes(&mut futures, &mut statuses, &mut wakes)
		.await;

	assert!(futures.is_empty());
	assert!(wakes.is_empty());

	Ok(())
}

#[tokio::test]
async fn zero_timing_bounds_still_arm_a_retry() -> Result {
	let config = Figment::new()
		.merge(("sender_timeout", 0))
		.merge(("sender_retry_backoff_limit", 0));

	let Some(fixture) = service_fixture(config).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let server: OwnedServerName = "zero.example".try_into()?;
	let dest = Destination::Federation(server.clone());
	let mut futures = SendingFutures::new(); // response state out-param
	let mut statuses: TransactionStatuses =
		[(dest.clone(), TransactionStatus::Running { tries: 0 })].into();

	let mut wakes = WakeQueue::new(); // response state out-param
	let started = Instant::now();
	let rejection = ErrorBody::Json(Value::Null).into_error(StatusCode::FORBIDDEN);
	let response = completion(Err((dest.clone(), Error::Federation(server, rejection))));

	sending
		.handle_response(response, &mut futures, &mut statuses, &mut wakes)
		.await;

	assert_eq!(wakes.len(), 1);

	let Reverse((due, armed)) = wakes.peek().expect("zero-bound retry");

	assert_eq!(armed, &dest);
	assert!(due.duration_since(started) >= Duration::from_secs(1));
	assert!(due.duration_since(started) < Duration::from_secs(4));

	Ok(())
}

#[tokio::test]
async fn stale_wakes_replay_once_while_retrying() -> Result {
	let Some(fixture) = fixture(false, -1).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let dest = Destination::Federation("stale.example".try_into()?);
	let queued = enqueue(sending, &dest, SendingEvent::Pdu(pdu_id(1)));
	let mut futures = SendingFutures::new(); // wake state out-param
	let mut statuses = [(dest.clone(), TransactionStatus::Retrying { tries: 3 })].into();
	let mut wakes =
		[Reverse((Instant::now(), dest.clone())), Reverse((Instant::now(), dest.clone()))]
			.into_iter()
			.collect();

	sending.db.mark_as_active(once(&queued));
	sending
		.drain_due_wakes(&mut futures, &mut statuses, &mut wakes)
		.await;

	assert_eq!(futures.len(), 1);
	assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Running { tries: 3 })));
	assert!(wakes.is_empty());

	Ok(())
}
