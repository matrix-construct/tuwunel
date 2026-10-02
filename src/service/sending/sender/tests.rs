mod appservice;
mod cleanup;
mod fixture;
mod ordering;
mod recovery;
mod startup;

use std::iter::once;

use futures::StreamExt;
use http::StatusCode;
use ruma::{OwnedServerName, api::error::ErrorBody};
use serde_json::Value;
use tuwunel_core::{Err, Error, Result};

use self::fixture::fixture;
use super::{
	NewEvents, SendingFutures, TransactionStatus, TransactionStatuses, WakeQueue,
	dispatch::{Completion, SendingResult},
	select::Selection,
};
use crate::{
	sending::{
		Destination, SendingEvent, Service,
		data::{Keys, QueueItem},
	},
	test_utils::pdu_id,
};

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
		let mut statuses = TransactionStatuses::new();

		sending
			.startup_netburst(0, &mut futures, &mut statuses, &mut WakeQueue::new())
			.await;

		assert!(futures.is_empty());
		assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Pending)));

		let payload = || [successor.clone()].into();
		let events = sending
			.select_events(&dest, payload(), &mut statuses)
			.await?;

		assert_eq!(events, Selection::Events(vec![old.clone()]));
		assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Running { tries: 0 })));
		assert!(
			sending
				.select_events(&dest, payload(), &mut statuses)
				.await?
				.eq(&Selection::Busy)
		);

		queued.exists(&successor.0).await?;
		active.exists(&old.0).await?;

		sending
			.handle_response(
				delivered(&dest, vec![old.0.clone()]),
				&mut futures,
				&mut statuses,
				&mut WakeQueue::new(),
			)
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
	let mut statuses = TransactionStatuses::new();

	sending
		.db
		.mark_as_active([first.clone(), second.clone()].iter());

	sending
		.startup_netburst(0, &mut futures, &mut statuses, &mut WakeQueue::new())
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
		.select_events(&dest, NewEvents::new(), &mut statuses)
		.await?;

	assert_eq!(events, Selection::Events(vec![first]));

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
	let mut statuses = TransactionStatuses::new();

	sending.db.mark_as_active(once(&old));
	sending
		.startup_netburst(0, &mut futures, &mut statuses, &mut WakeQueue::new())
		.await;

	assert_eq!(futures.len(), 1);
	assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Running { tries: 0 })));
	assert!(
		sending
			.select_events(&dest, NewEvents::new(), &mut statuses)
			.await?
			.eq(&Selection::Busy)
	);

	sending.db.db["servercurrentevent_data"]
		.exists(&old.0)
		.await?;

	Ok(())
}

#[tokio::test]
async fn zero_keep_drops_every_active_row_without_redelivery() -> Result {
	let Some(fixture) = fixture(true, 0).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let active = &sending.db.db["servercurrentevent_data"];
	let destinations = [
		Destination::Appservice("zero".into()),
		Destination::Push("@u:localhost".try_into()?, "zero".into()),
	];

	let rows: Vec<_> = destinations
		.iter()
		.map(|dest| enqueue(sending, dest, SendingEvent::Pdu(pdu_id(1))))
		.collect();

	let mut futures = SendingFutures::new(); // startup_netburst &mut out-param
	let mut statuses = TransactionStatuses::new(); // startup_netburst &mut out-param

	sending.db.mark_as_active(rows.iter());
	sending
		.startup_netburst(0, &mut futures, &mut statuses, &mut WakeQueue::new())
		.await;

	assert!(futures.is_empty());
	assert!(statuses.is_empty());
	for (key, _) in &rows {
		assert!(
			active
				.get(key)
				.await
				.is_err_and(|error| error.is_not_found())
		);
	}

	Ok(())
}

#[tokio::test]
async fn failure_streak_survives_replays() -> Result {
	let Some(fixture) = fixture(false, -1).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let server: OwnedServerName = "remote.example".try_into()?;
	let dest = Destination::Federation(server.clone());
	let head = SendingEvent::Pdu(pdu_id(1));
	let mut futures = SendingFutures::new();
	let mut statuses = TransactionStatuses::new(); // handle_response takes &mut state
	let mut wakes = WakeQueue::new(); // handle_response takes &mut state

	for (before, after, id) in [(0, 1, 1), (1, 2, 2), (2, 3, 3)] {
		let queued = enqueue(sending, &dest, SendingEvent::Pdu(pdu_id(id)));
		let events = sending
			.select_events(&dest, [queued].into(), &mut statuses)
			.await?;

		assert!(matches!(events, Selection::Events(items) if carries(&items, &head)));
		assert!(matches!(
			statuses.get(&dest),
			Some(&TransactionStatus::Running { tries }) if tries == before
		));

		let rejection = ErrorBody::Json(Value::Null).into_error(StatusCode::FORBIDDEN);
		let rejected = Err((dest.clone(), Error::Federation(server.clone(), rejection)));

		sending
			.handle_response(completion(rejected), &mut futures, &mut statuses, &mut wakes)
			.await;

		assert!(matches!(
			statuses.get(&dest),
			Some(&TransactionStatus::Retrying { tries }) if tries == after
		));
	}

	let Selection::Events(items) = sending
		.select_events(&dest, NewEvents::new(), &mut statuses)
		.await?
	else {
		return Err!("the rejected batch replays");
	};

	assert!(carries(&items, &head));
	assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Running { tries: 3 })));

	let keys = items.into_iter().map(|(key, _)| key).collect();

	sending
		.handle_response(delivered(&dest, keys), &mut futures, &mut statuses, &mut wakes)
		.await;

	let active: Vec<_> = sending
		.db
		.active_requests_for(&dest)
		.collect()
		.await;

	assert!(!carries(&active, &head));
	assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Running { tries: 0 })));

	Ok(())
}

pub(super) fn enqueue(sending: &Service, dest: &Destination, event: SendingEvent) -> QueueItem {
	let key = sending
		.db
		.queue_requests(once((&event, dest)))
		.pop()
		.expect("one queued event");

	(key, event)
}

pub(super) fn delivered(dest: &Destination, keys: Keys) -> Completion {
	Completion { keys, ..completion(Ok(dest.clone())) }
}

fn carries(items: &[QueueItem], event: &SendingEvent) -> bool {
	items.iter().any(|(_, carried)| carried == event)
}

fn completion(result: SendingResult) -> Completion { Completion { result, keys: Keys::new() } }
