use std::{
	cmp::Reverse,
	iter::once,
	time::{Duration, SystemTime},
};

use futures::StreamExt;
use ruma::OwnedServerName;
use tokio::time::Instant;
use tuwunel_core::{Config, Result, config::Figment, err};

use super::{
	super::{NewEvents, Selection},
	SendingFutures, TransactionStatus, TransactionStatuses, WakeQueue, completion, enqueue,
	fixture, pdu_id, service_fixture,
};
use crate::{
	federation::{Classification, ShouldAttempt},
	sending::{Destination, Msg, SendingEvent, Service},
	test_utils::Fixture,
};

#[tokio::test]
async fn forbidden_server_stays_queued_without_a_send() -> Result {
	let forbidden = [r"^refused\.example$"];
	let config = Figment::new().merge(("federation.forbidden_remote_server_names", forbidden));

	refused_destination_stays_queued(config).await
}

#[tokio::test]
async fn disabled_federation_stays_queued_without_a_send() -> Result {
	let config = Figment::new().merge(("federation.enable", false));

	refused_destination_stays_queued(config).await
}

async fn refused_destination_stays_queued(config: Figment) -> Result {
	let config = config.merge(("sending.workers", 1));
	let Some(fixture) = service_fixture(config).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let federation = &fixture.services.federation;
	let server: OwnedServerName = "refused.example".try_into()?;
	let dest = Destination::Federation(server.clone());
	let mut futures = SendingFutures::new(); // request state out-param
	let mut statuses = TransactionStatuses::new(); // request state out-param
	let mut wakes = WakeQueue::new(); // request state out-param
	let started = Instant::now();

	assert_traffic_refused(sending, &dest, &mut futures, &mut statuses, &mut wakes).await;
	assert_eq!(sending.db.queued_requests(&dest).count().await, 3);
	assert!(!federation.peer_has_failures(&server).await);

	let window = Duration::from_secs(sending.server.config.sending.timeout);

	assert_refusal_wake(&mut wakes, &dest, started, window);

	Ok(())
}

async fn assert_traffic_refused<'a>(
	sending: &'a Service,
	dest: &Destination,
	futures: &mut SendingFutures<'a>,
	statuses: &mut TransactionStatuses,
	wakes: &mut WakeQueue,
) {
	let queued = (1..=3).map(|count| enqueue(sending, dest, SendingEvent::Pdu(pdu_id(count))));
	let flush = (Vec::new(), SendingEvent::Flush);

	for (queue_id, event) in queued.chain([flush]) {
		let msg = Msg { dest: dest.clone(), event, queue_id };

		sending
			.handle_request(msg, futures, statuses, wakes)
			.await;

		assert!(futures.is_empty());
		assert!(statuses.is_empty());
		assert_eq!(wakes.len(), 1);
	}
}

fn assert_refusal_wake(
	wakes: &mut WakeQueue,
	dest: &Destination,
	started: Instant,
	window: Duration,
) {
	let Reverse((deadline, armed)) = wakes.pop().expect("refusal wake");
	let elapsed = deadline.duration_since(started);
	let earliest = window.saturating_sub(Duration::from_secs(1));
	let latest = window
		.saturating_mul(2)
		.saturating_add(Duration::from_secs(2));

	assert_eq!(armed, *dest);
	assert!(elapsed >= earliest);
	assert!(elapsed < latest);
}

#[tokio::test]
async fn reenabled_federation_resumes_refused_destinations() -> Result {
	let Some(fixture) = fixture(false, -1).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let retrying = Destination::Federation("retrying.example".try_into()?);
	let queued = Destination::Federation("queued.example".try_into()?);
	let active = enqueue(sending, &retrying, SendingEvent::Pdu(pdu_id(1)));
	let (queue_id, event) = enqueue(sending, &queued, SendingEvent::Pdu(pdu_id(2)));
	let msg = Msg { dest: queued.clone(), event, queue_id };
	let mut futures = SendingFutures::new(); // wake state out-param
	let mut statuses = [(retrying.clone(), TransactionStatus::Retrying { tries: 1 })].into();
	let mut wakes = WakeQueue::from([Reverse((Instant::now(), retrying.clone()))]);

	sending.db.mark_as_active(once(&active));
	set_federation_enabled(&fixture, false)?;

	sending
		.handle_request(msg, &mut futures, &mut statuses, &mut wakes)
		.await;

	sending
		.drain_due_wakes(&mut futures, &mut statuses, &mut wakes)
		.await;

	assert!(futures.is_empty());
	assert_eq!(wakes.len(), 2);
	assert!(matches!(
		statuses.get(&retrying),
		Some(TransactionStatus::Retrying { tries: 1 })
	));

	set_federation_enabled(&fixture, true)?;
	wakes = due_now(wakes);

	sending
		.drain_due_wakes(&mut futures, &mut statuses, &mut wakes)
		.await;

	assert_eq!(futures.len(), 2);
	assert!(wakes.is_empty());
	assert!(matches!(statuses.get(&retrying), Some(TransactionStatus::Running { tries: 1 })));
	assert!(matches!(statuses.get(&queued), Some(TransactionStatus::Running { tries: 0 })));

	Ok(())
}

fn set_federation_enabled(fixture: &Fixture, enable: bool) -> Result {
	let mut config = Config::clone(&fixture.services.server.config);

	config.federation.enable = enable;
	fixture.services.config.replace(config)?;

	Ok(())
}

fn due_now(wakes: WakeQueue) -> WakeQueue {
	wakes
		.into_iter()
		.map(|Reverse((_, dest))| Reverse((Instant::now(), dest)))
		.collect()
}

#[tokio::test]
async fn refused_traffic_arms_one_wake_and_preserves_status() -> Result {
	let Some(fixture) = fixture(false, -1).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let dest = Destination::Federation("refused.example".try_into()?);
	let Destination::Federation(server) = &dest else {
		unreachable!();
	};

	let peer = &fixture.services.federation;

	peer.record_failure(server, Classification::Transient);

	let ShouldAttempt::No { earliest_retry } = peer.should_attempt(server).await else {
		panic!("fixture peer refuses traffic");
	};

	let started = Instant::now();
	let remaining = earliest_retry
		.duration_since(SystemTime::now())
		.unwrap_or_default();

	let mut futures = SendingFutures::new(); // request state out-param
	let mut statuses = TransactionStatuses::new(); // request state out-param
	let mut wakes = WakeQueue::new(); // request state out-param

	for status in
		[TransactionStatus::Running { tries: 2 }, TransactionStatus::RunningForceRetry {
			tries: 2,
		}] {
		statuses.insert(dest.clone(), status);

		let selection = sending
			.select_events(&dest, NewEvents::new(), &mut statuses)
			.await?;

		assert!(matches!(selection, Selection::Busy));
		assert!(matches!(
			statuses.get(&dest),
			Some(
				TransactionStatus::Running { tries: 2 }
					| TransactionStatus::RunningForceRetry { tries: 2 }
			)
		));

		assert!(futures.is_empty());
		assert!(wakes.is_empty());
	}

	statuses.remove(&dest);

	assert_traffic_refused(sending, &dest, &mut futures, &mut statuses, &mut wakes).await;
	assert_refusal_wake(&mut wakes, &dest, started, remaining);

	peer.record_success(server).await;
	let verdict = peer.should_attempt(server).await;

	assert!(matches!(verdict, ShouldAttempt::Yes));
	peer.record_failure(server, Classification::Transient);
	statuses.insert(dest.clone(), TransactionStatus::Retrying { tries: 2 });

	let msg = Msg {
		dest: dest.clone(),
		event: SendingEvent::Flush,
		queue_id: Vec::new(),
	};

	sending
		.handle_request(msg, &mut futures, &mut statuses, &mut wakes)
		.await;

	assert_eq!(wakes.len(), 1);
	assert!(futures.is_empty());
	assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Retrying { tries: 2 })));

	peer.record_success(server).await;
	statuses.insert(dest.clone(), TransactionStatus::Running { tries: 2 });
	wakes.clear();

	sending
		.handle_response(completion(Ok(dest.clone())), &mut futures, &mut statuses, &mut wakes)
		.await;

	assert_eq!(futures.len(), 1);
	assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Running { tries: 0 })));

	futures.clear();
	let failed = completion(Err((dest.clone(), err!("simulated delivery failure"))));

	sending
		.handle_response(failed, &mut futures, &mut statuses, &mut wakes)
		.await;

	assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Retrying { tries: 1 })));
	assert_eq!(wakes.len(), 1);

	Ok(())
}
