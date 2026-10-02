use std::{cmp::Reverse, iter::once, time::Duration};

use futures::StreamExt;
use tokio::time::{Instant, sleep_until};
use tuwunel_core::{Error, Result};

use super::{
	SendingFutures, TransactionStatus, WakeQueue, completion, enqueue, fixture::fixture, pdu_id,
};
use crate::sending::{Destination, SendingEvent};

#[tokio::test]
async fn missing_appservice_registration_retries_on_its_timer_and_preserves_rows() -> Result {
	let Some(fixture) = fixture(false, -1).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let dest = Destination::Appservice("missing-registration".into());
	let old = enqueue(sending, &dest, SendingEvent::Pdu(pdu_id(1)));

	sending.db.mark_as_active(once(&old));

	let successor = enqueue(sending, &dest, SendingEvent::Pdu(pdu_id(2)));
	let mut futures = SendingFutures::new(); // response and wake state out-param
	let mut statuses = [(dest.clone(), TransactionStatus::Running { tries: 0 })].into();
	let mut wakes = WakeQueue::new(); // response and wake state out-param
	let response = sending
		.send_events(dest.clone(), vec![old.clone()], None)
		.await;

	response
		.result
		.as_ref()
		.expect_err("appservice send fails");

	let started = Instant::now();

	sending
		.handle_response(response, &mut futures, &mut statuses, &mut wakes)
		.await;

	let finished = Instant::now();
	let Reverse((due, armed)) = wakes.peek().expect("appservice timer");
	let due = *due;

	assert_eq!(armed, &dest);
	assert!(due >= started + Duration::from_secs(2));
	assert!(due <= finished + Duration::from_secs(4));
	assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Retrying { tries: 1 })));
	assert!(futures.is_empty());

	sending
		.drain_due_wakes(&mut futures, &mut statuses, &mut wakes)
		.await;

	assert!(futures.is_empty());
	assert_eq!(wakes.len(), 1);

	sleep_until(due).await;
	sending
		.drain_due_wakes(&mut futures, &mut statuses, &mut wakes)
		.await;

	assert!(wakes.is_empty());
	assert_eq!(futures.len(), 1);
	assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Running { tries: 1 })));

	let response = futures
		.next()
		.await
		.expect("timer retry response");

	response
		.result
		.as_ref()
		.expect_err("appservice send fails");

	sending
		.handle_response(response, &mut futures, &mut statuses, &mut wakes)
		.await;

	assert!(futures.is_empty());
	assert_eq!(wakes.len(), 1);
	assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Retrying { tries: 2 })));
	sending.db.db["servercurrentevent_data"]
		.exists(&old.0)
		.await?;

	sending.db.db["servernameevent_data"]
		.exists(&successor.0)
		.await?;

	Ok(())
}

#[tokio::test]
async fn ping_during_flight_retries_immediately_after_failure_and_stale_timer_is_safe() -> Result
{
	let Some(fixture) = fixture(false, -1).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let id = "forced-registration";
	let dest = Destination::Appservice(id.into());
	let old = enqueue(sending, &dest, SendingEvent::Pdu(pdu_id(1)));

	sending.db.mark_as_active(once(&old));

	let successor = enqueue(sending, &dest, SendingEvent::Pdu(pdu_id(2)));
	let mut futures = SendingFutures::new(); // request and response state out-param
	let mut statuses = [(dest.clone(), TransactionStatus::Running { tries: 0 })].into();
	let mut wakes = WakeQueue::new(); // request and response state out-param

	sending.flush_appservice(id.into())?;

	let msg = sending.channels[0]
		.1
		.try_recv()
		.expect("successful ping flush");

	sending
		.handle_request(msg, &mut futures, &mut statuses, &mut wakes)
		.await;

	assert!(futures.is_empty());
	assert!(matches!(
		statuses.get(&dest),
		Some(TransactionStatus::RunningForceRetry { tries: 0 })
	));

	let response = sending
		.send_events(dest.clone(), vec![old.clone()], None)
		.await;

	response
		.result
		.as_ref()
		.expect_err("appservice send fails");

	sending
		.handle_response(response, &mut futures, &mut statuses, &mut wakes)
		.await;

	assert_eq!(futures.len(), 1);
	assert_eq!(wakes.len(), 1);
	assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Running { tries: 1 })));
	let due = wakes
		.peek()
		.expect("timer after forced retry")
		.0
		.0;

	assert!(due > Instant::now());

	sending.db.db["servercurrentevent_data"]
		.exists(&old.0)
		.await?;

	sending.db.db["servernameevent_data"]
		.exists(&successor.0)
		.await?;

	wakes.clear();
	wakes.push(Reverse((Instant::now(), dest.clone())));
	sending
		.drain_due_wakes(&mut futures, &mut statuses, &mut wakes)
		.await;

	assert!(wakes.is_empty());
	assert_eq!(futures.len(), 1);
	assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Running { tries: 1 })));

	Ok(())
}

#[tokio::test]
async fn very_large_appservice_failure_streak_is_capped_and_deduped() -> Result {
	let Some(fixture) = fixture(false, -1).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let dest = Destination::Appservice("large-streak".into());
	let mut futures = SendingFutures::new(); // response state out-param
	let mut statuses = [(dest.clone(), TransactionStatus::Running { tries: u32::MAX })].into();
	let mut wakes = WakeQueue::new(); // response state out-param
	let started = Instant::now();
	let response =
		completion(Err((dest.clone(), Error::bad_database("appservice fixture failure"))));

	let response = sending.handle_response(response, &mut futures, &mut statuses, &mut wakes);

	eprintln!("appservice response future={} bytes", size_of_val(&response));
	response.await;

	let finished = Instant::now();
	let Reverse((due, armed)) = wakes.peek().expect("capped appservice retry");
	let due = *due;

	assert_eq!(armed, &dest);
	assert!(due >= started + Duration::from_secs(512));
	assert!(due <= finished + Duration::from_secs(1023));
	assert!(matches!(
		statuses.get(&dest),
		Some(TransactionStatus::Retrying { tries: u32::MAX })
	));

	statuses.insert(dest.clone(), TransactionStatus::Running { tries: u32::MAX });

	let response = completion(Err((dest, Error::bad_database("appservice fixture failure"))));

	sending
		.handle_response(response, &mut futures, &mut statuses, &mut wakes)
		.await;

	assert_eq!(wakes.len(), 1);
	assert_eq!(wakes.peek().expect("same retry").0.0, due);
	assert!(futures.is_empty());

	Ok(())
}
