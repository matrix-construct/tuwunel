mod pacing;
mod performance;
mod shard;

use std::{
	cmp::Reverse,
	iter::once,
	time::{Duration, SystemTime},
};

use tokio::time::Instant;
use tuwunel_core::{Result, config::Figment};

use super::{SendingFutures, TransactionStatus, TransactionStatuses, WakeQueue, enqueue, pdu_id};
use crate::{
	federation::{Classification, ShouldAttempt},
	sending::{Destination, SendingEvent},
	test_utils::fixture,
};

#[tokio::test]
async fn boot_arms_pending_and_queued_work_without_losing_active_ownership() -> Result {
	let Some(fixture) = fixture(config(false, 0)).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let pending = destination("pending.example");
	let queued = destination("queued.example");
	let blocked = destination("blocked.example");
	let armed = destination("armed.example");
	let old = enqueue(sending, &pending, SendingEvent::Pdu(pdu_id(1)));

	sending.db.mark_as_active(once(&old));

	let successor = enqueue(sending, &pending, SendingEvent::Pdu(pdu_id(2)));
	let queued_row = enqueue(sending, &queued, SendingEvent::Pdu(pdu_id(3)));

	enqueue(sending, &blocked, SendingEvent::Pdu(pdu_id(4)));
	enqueue(sending, &armed, SendingEvent::Pdu(pdu_id(5)));
	enqueue(sending, &Destination::Appservice("bridge".into()), SendingEvent::Pdu(pdu_id(6)));
	enqueue(
		sending,
		&Destination::Push("@user:localhost".try_into()?, "key".into()),
		SendingEvent::BadgeRefresh,
	);

	let Destination::Federation(server) = &blocked else {
		unreachable!();
	};

	fixture
		.services
		.federation
		.record_failure(server, Classification::Transient);

	let ShouldAttempt::No { earliest_retry } = fixture
		.services
		.federation
		.should_attempt(server)
		.await
	else {
		panic!("gate remains closed");
	};

	let remaining = earliest_retry
		.duration_since(SystemTime::now())
		.unwrap_or_default();

	let existing = Instant::now() + Duration::from_secs(100);
	let mut wakes = [Reverse((existing, armed.clone()))].into();
	let mut futures = SendingFutures::new(); // startup and wake state out-param
	let mut statuses = TransactionStatuses::new(); // startup and wake state out-param
	let started = Instant::now();

	sending
		.startup_netburst(0, &mut futures, &mut statuses, &mut wakes)
		.await;

	let finished = Instant::now();

	assert!(futures.is_empty());
	assert_eq!(wakes.len(), 4);
	assert!(matches!(statuses.get(&pending), Some(TransactionStatus::Pending)));
	assert!(!statuses.contains_key(&queued));
	assert!(!statuses.contains_key(&blocked));
	assert!(!statuses.contains_key(&armed));
	assert_eq!(deadline(&wakes, &armed), existing);
	assert!(deadline(&wakes, &pending) >= started + Duration::from_secs(1));
	assert!(deadline(&wakes, &pending) < finished + Duration::from_secs(4));
	assert!(deadline(&wakes, &queued) >= started + Duration::from_secs(2));
	assert!(deadline(&wakes, &queued) < finished + Duration::from_secs(5));
	assert!(deadline(&wakes, &blocked) >= started + remaining.saturating_sub(finished - started));
	assert!(
		deadline(&wakes, &blocked)
			< finished + remaining.saturating_mul(2) + Duration::from_secs(3)
	);

	wakes.clear();
	wakes.push(Reverse((Instant::now(), queued.clone())));
	sending
		.drain_due_wakes(&mut futures, &mut statuses, &mut wakes)
		.await;

	assert_eq!(futures.len(), 1);
	assert!(matches!(statuses.get(&queued), Some(TransactionStatus::Running { tries: 0 })));
	sending.db.db["servercurrentevent_data"]
		.exists(&queued_row.0)
		.await?;

	sending.db.db["servercurrentevent_data"]
		.exists(&old.0)
		.await?;

	sending.db.db["servernameevent_data"]
		.exists(&successor.0)
		.await?;

	futures.clear();
	wakes.push(Reverse((Instant::now(), pending.clone())));
	sending
		.drain_due_wakes(&mut futures, &mut statuses, &mut wakes)
		.await;

	assert_eq!(futures.len(), 1);
	assert!(matches!(statuses.get(&pending), Some(TransactionStatus::Running { tries: 0 })));
	sending.db.db["servercurrentevent_data"]
		.exists(&old.0)
		.await?;

	sending.db.db["servernameevent_data"]
		.exists(&successor.0)
		.await?;

	Ok(())
}

#[tokio::test]
async fn netburst_keeps_active_generations_and_arms_only_queued_federation() -> Result {
	let Some(fixture) = fixture(config(true, 0)).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let running = destination("running.example");
	let queued = destination("queued.example");
	let blocked = destination("blocked.example");
	let old = enqueue(sending, &running, SendingEvent::Pdu(pdu_id(1)));

	sending.db.mark_as_active(once(&old));

	let successor = enqueue(sending, &running, SendingEvent::Pdu(pdu_id(2)));

	enqueue(sending, &queued, SendingEvent::Pdu(pdu_id(3)));
	enqueue(sending, &blocked, SendingEvent::Pdu(pdu_id(4)));

	for dest in [&running, &blocked] {
		let Destination::Federation(server) = dest else {
			unreachable!();
		};

		fixture
			.services
			.federation
			.record_failure(server, Classification::Transient);

		let verdict = fixture
			.services
			.federation
			.should_attempt(server)
			.await;

		assert!(matches!(verdict, ShouldAttempt::No { .. }));
	}

	let mut futures = SendingFutures::new(); // startup state out-param
	let mut statuses = TransactionStatuses::new(); // startup state out-param
	let mut wakes = WakeQueue::new(); // startup state out-param

	sending
		.startup_netburst(0, &mut futures, &mut statuses, &mut wakes)
		.await;

	assert_eq!(futures.len(), 1);
	assert!(matches!(statuses.get(&running), Some(TransactionStatus::Running { tries: 0 })));
	assert!(!statuses.contains_key(&queued));
	assert_eq!(wakes.len(), 2);
	let running_armed = wakes
		.iter()
		.any(|Reverse((_, dest))| dest == &running);

	assert!(!running_armed);

	assert!(
		wakes
			.iter()
			.any(|Reverse((_, dest))| dest == &queued)
	);

	assert!(
		wakes
			.iter()
			.any(|Reverse((_, dest))| dest == &blocked)
	);

	sending.db.db["servercurrentevent_data"]
		.exists(&old.0)
		.await?;

	sending.db.db["servernameevent_data"]
		.exists(&successor.0)
		.await?;

	Ok(())
}

#[tokio::test]
async fn zero_keep_leaves_badges_queued_and_arms_queued_federation() -> Result {
	let config = config(true, 0).merge(("startup_netburst_keep", 0));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let dropped = destination("dropped.example");
	let queued = destination("queued.example");
	let old = enqueue(sending, &dropped, SendingEvent::Pdu(pdu_id(1)));

	sending.db.mark_as_active(once(&old));
	enqueue(sending, &queued, SendingEvent::Pdu(pdu_id(2)));

	let push = Destination::Push("@user:localhost".try_into()?, "key".into());
	let badge = enqueue(sending, &push, SendingEvent::BadgeRefresh);
	let mut futures = SendingFutures::new(); // startup state out-param
	let mut statuses = TransactionStatuses::new(); // startup state out-param
	let mut wakes = WakeQueue::new(); // startup state out-param

	sending
		.startup_netburst(0, &mut futures, &mut statuses, &mut wakes)
		.await;

	assert!(futures.is_empty());
	assert!(statuses.is_empty());
	assert_eq!(wakes.len(), 1);
	assert_eq!(wakes.peek().expect("queued wake").0.1, queued);
	assert!(
		sending.db.db["servercurrentevent_data"]
			.get(&old.0)
			.await
			.is_err_and(|error| error.is_not_found())
	);

	sending.db.db["servernameevent_data"]
		.exists(&badge.0)
		.await?;

	Ok(())
}

#[tokio::test]
async fn maintenance_preserves_rows_without_arming_or_sending() -> Result {
	let config = config(false, 0).merge(("maintenance", true));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let pending = destination("pending.example");
	let queued = destination("queued.example");
	let old = enqueue(sending, &pending, SendingEvent::Pdu(pdu_id(1)));

	sending.db.mark_as_active(once(&old));

	let new = enqueue(sending, &queued, SendingEvent::Pdu(pdu_id(2)));
	let mut futures = SendingFutures::new(); // startup state out-param
	let mut statuses = TransactionStatuses::new(); // startup state out-param
	let mut wakes = WakeQueue::new(); // startup state out-param

	sending
		.startup_netburst(0, &mut futures, &mut statuses, &mut wakes)
		.await;

	assert!(futures.is_empty());
	assert!(wakes.is_empty());
	assert!(matches!(statuses.get(&pending), Some(TransactionStatus::Pending)));
	assert!(!statuses.contains_key(&queued));
	sending.db.db["servercurrentevent_data"]
		.exists(&old.0)
		.await?;

	sending.db.db["servernameevent_data"]
		.exists(&new.0)
		.await?;

	Ok(())
}

fn config(netburst: bool, timeout: u64) -> Figment {
	Figment::new()
		.merge(("startup_netburst", netburst))
		.merge(("startup_netburst_keep", -1))
		.merge(("sender_timeout", timeout))
		.merge(("sender_workers", 1))
}

fn destination(name: &str) -> Destination {
	Destination::Federation(name.try_into().expect("server name"))
}

fn deadline(wakes: &WakeQueue, dest: &Destination) -> Instant {
	wakes
		.iter()
		.find(|Reverse((_, armed))| armed == dest)
		.map(|Reverse((due, _))| *due)
		.expect("destination wake")
}
