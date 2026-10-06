use std::{
	cmp::Reverse,
	time::{Duration, SystemTime},
};

use tokio::time::Instant;
use tuwunel_core::{Result, err};

use super::{
	super::{NewEvents, Selection},
	SendingFutures, TransactionStatus, TransactionStatuses, WakeQueue, completion, enqueue,
	fixture, pdu_id,
};
use crate::{
	federation::{Classification, ShouldAttempt},
	sending::{Destination, Msg, SendingEvent},
};

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

	for count in 1..=3 {
		let (queue_id, event) = enqueue(sending, &dest, SendingEvent::Pdu(pdu_id(count)));
		let msg = Msg { dest: dest.clone(), event, queue_id };

		sending
			.handle_request(msg, &mut futures, &mut statuses, &mut wakes)
			.await;

		assert!(futures.is_empty());
		assert!(statuses.is_empty());
		assert_eq!(wakes.len(), 1);
	}

	let Reverse((deadline, armed)) = wakes.pop().expect("refusal wake");

	assert_eq!(armed, dest);
	assert!(deadline.duration_since(started) >= remaining.saturating_sub(Duration::from_secs(1)));
	assert!(
		deadline.duration_since(started) < remaining.saturating_mul(2) + Duration::from_secs(2)
	);

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
