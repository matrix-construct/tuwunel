use std::iter::once;

use tuwunel_core::Result;

use super::{
	super::test_utils::{fixture, pdu_id},
	CurTransactionStatus, Destination, QueueItem, SendingEvent, SendingFutures, Service,
	TransactionStatus,
};

mod cleanup;

#[tokio::test]
async fn restart_replays_active_before_queued_successors() -> Result {
	let Some(fixture) = fixture(false, -1).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let active = &sending.db.db["servercurrentevent_data"];
	let queued = &sending.db.db["servernameevent_data"];
	let old_id = pdu_id(1);
	let new_id = pdu_id(2);
	let destinations = [
		Destination::Appservice("restart".into()),
		Destination::Push("@u:localhost".try_into()?, "key".into()),
	];

	for dest in destinations {
		let old = enqueue(sending, &dest, SendingEvent::Pdu(old_id));

		sending.db.mark_as_active(once(&old));

		let successor = enqueue(sending, &dest, SendingEvent::Pdu(new_id));
		let mut futures = SendingFutures::new();
		let mut statuses = CurTransactionStatus::new();

		sending
			.startup_netburst(0, &mut futures, &mut statuses)
			.await;

		assert!(futures.is_empty());
		assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Pending)));

		let payload = || vec![successor.clone()];
		let events = sending
			.select_events(&dest, payload(), &mut statuses)
			.await?;

		assert_eq!(events, Some(vec![SendingEvent::Pdu(old_id)]));
		assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Running)));
		assert!(
			sending
				.select_events(&dest, payload(), &mut statuses)
				.await?
				.is_none()
		);

		queued.exists(&successor.0).await?;
		active.exists(&old.0).await?;

		sending
			.handle_response_ok(&dest, &mut futures, &mut statuses)
			.await;

		assert_eq!(futures.len(), 1);
		assert!(
			active
				.get(&old.0)
				.await
				.is_err_and(|error| error.is_not_found())
		);

		active.exists(&successor.0).await?;
		assert!(
			queued
				.get(&successor.0)
				.await
				.is_err_and(|error| error.is_not_found())
		);

		active.remove(&successor.0);
	}

	Ok(())
}

#[tokio::test]
async fn restart_retains_the_configured_active_limit() -> Result {
	let Some(fixture) = fixture(false, 1).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let dest = Destination::Appservice("trim".into());
	let first_id = pdu_id(1);
	let first = enqueue(sending, &dest, SendingEvent::Pdu(first_id));
	let second = enqueue(sending, &dest, SendingEvent::Pdu(pdu_id(2)));
	let active = &sending.db.db["servercurrentevent_data"];

	let mut futures = SendingFutures::new();
	let mut statuses = CurTransactionStatus::new();

	sending
		.db
		.mark_as_active([first.clone(), second.clone()].iter());

	sending
		.startup_netburst(0, &mut futures, &mut statuses)
		.await;

	assert!(futures.is_empty());
	assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Pending)));
	active.exists(&first.0).await?;
	assert!(
		active
			.get(&second.0)
			.await
			.is_err_and(|error| error.is_not_found())
	);

	let events = sending
		.select_events(&dest, Vec::new(), &mut statuses)
		.await?;

	assert_eq!(events, Some(vec![SendingEvent::Pdu(first_id)]));

	Ok(())
}

#[tokio::test]
async fn enabled_netburst_keeps_active_ownership() -> Result {
	let Some(fixture) = fixture(true, -1).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let dest = Destination::Appservice("netburst".into());
	let old = enqueue(sending, &dest, SendingEvent::Pdu(pdu_id(1)));
	let mut futures = SendingFutures::new();
	let mut statuses = CurTransactionStatus::new();

	sending.db.mark_as_active(once(&old));
	sending
		.startup_netburst(0, &mut futures, &mut statuses)
		.await;

	assert_eq!(futures.len(), 1);
	assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Running)));
	assert!(
		sending
			.select_events(&dest, Vec::new(), &mut statuses)
			.await?
			.is_none()
	);

	sending.db.db["servercurrentevent_data"]
		.exists(&old.0)
		.await?;

	Ok(())
}

fn enqueue(sending: &Service, dest: &Destination, event: SendingEvent) -> QueueItem {
	let key = sending
		.db
		.queue_requests(once((&event, dest)))
		.pop()
		.expect("one queued event");

	(key, event)
}
