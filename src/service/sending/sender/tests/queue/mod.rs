use std::iter::once;

use futures::{FutureExt, StreamExt};
use ruma::{room_id, user_id};
use tokio::sync::oneshot::channel;
use tuwunel_core::{Result, err};

use super::{
	Destination, SendingEvent, SendingFutures, TransactionStatus, WakeQueue, enqueue, fixture,
	pdu_id,
};
use crate::sending::{Msg, queue::Control};

#[tokio::test]
async fn push_queue_controls_drop_and_retry_without_duplicate_delivery() -> Result {
	let Some(fixture) = fixture(false, -1).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let pusher = &fixture.services.pusher;
	let user = user_id!("@u:localhost");
	let room = room_id!("!room:localhost");
	let dest = Destination::Push(user.to_owned(), "drop".into());
	let other = Destination::Push(user.to_owned(), "keep".into());
	let active = enqueue(sending, &dest, SendingEvent::Pdu(pdu_id(1)));

	sending.db.mark_as_active(once(&active));

	let (queue_id, event) = enqueue(sending, &dest, SendingEvent::Pdu(pdu_id(2)));
	let retained = enqueue(sending, &other, SendingEvent::Pdu(pdu_id(3)));
	let mut futures = SendingFutures::new();
	let mut statuses = [(dest.clone(), TransactionStatus::Pending)].into();
	let mut wakes = WakeQueue::new();
	let (done, response) = channel();
	let control = Control { dest: dest.clone(), drop: true, done };

	assert!(pusher.queue_suppressed_push(user, "drop", room, pdu_id(4)));

	assert!(pusher.queue_suppressed_push(user, "keep", room, pdu_id(5)));

	sending
		.handle_control(control, &mut futures, &mut statuses, &mut wakes)
		.await;

	response.await.expect("drop acknowledged")?;

	let queued = sending.db.queued_requests(&dest).count().await;

	assert_eq!(queued, 0);

	let active = sending
		.db
		.active_requests_for(&dest)
		.count()
		.await;

	assert_eq!(active, 0);

	let suppressed = pusher.take_suppressed_for_pushkey(user, "drop");
	let retained_suppressed = vec![(room.to_owned(), vec![pdu_id(5)])];

	assert_eq!(suppressed, Vec::new());
	assert_eq!(pusher.take_suppressed_for_pushkey(user, "keep"), retained_suppressed);

	let queued = sending
		.db
		.queued_requests(&other)
		.collect::<Vec<_>>()
		.await;

	assert_eq!(queued, vec![retained]);

	let stale = Msg { dest: dest.clone(), event, queue_id };

	sending
		.handle_request(stale, &mut futures, &mut statuses, &mut wakes)
		.await;

	assert!(futures.is_empty());

	let active = sending
		.db
		.active_requests_for(&dest)
		.count()
		.await;

	assert_eq!(active, 0);

	assert!(!statuses.contains_key(&dest));

	let retry = enqueue(sending, &dest, SendingEvent::Pdu(pdu_id(6)));

	for _ in 0..2 {
		let (done, response) = channel();
		let control = Control { dest: dest.clone(), drop: false, done };

		sending
			.handle_control(control, &mut futures, &mut statuses, &mut wakes)
			.await;

		response.await.expect("retry acknowledged")?;

		assert_eq!(futures.len(), 1);
		let queued = sending.db.queued_requests(&dest).count().await;

		assert_eq!(queued, 0);
		let active = sending
			.db
			.active_requests_for(&dest)
			.collect::<Vec<_>>()
			.await;

		assert_eq!(active, vec![retry.clone()]);
	}

	let waking = Destination::Push(user.to_owned(), "wake".into());
	let pending = enqueue(sending, &waking, SendingEvent::Pdu(pdu_id(7)));

	statuses.insert(waking.clone(), TransactionStatus::Pending);
	sending
		.handle_force_retry(waking.clone(), &mut futures, &mut statuses, &mut wakes)
		.await;

	let active = sending
		.db
		.active_requests_for(&waking)
		.collect::<Vec<_>>()
		.await;

	assert_eq!(active, vec![pending]);
	assert_eq!(futures.len(), 2);

	let (done, response) = channel();
	let queued = sending.controls[0]
		.0
		.send(Control { dest: waking, drop: false, done });

	queued.map_err(|error| err!("{error}"))?;
	sending.close();
	assert!(matches!(response.now_or_never(), Some(Err(_))));

	let closed = sending
		.retry_push_queue(user.to_owned(), "closed".into())
		.now_or_never();

	assert!(matches!(closed, Some(Err(_))));

	Ok(())
}
