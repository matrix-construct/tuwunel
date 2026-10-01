use std::iter::once;

use tuwunel_core::Result;

use super::{
	SendingFutures, TransactionStatus, TransactionStatuses, WakeQueue, enqueue, fixture, pdu_id,
};
use crate::sending::{Destination, SendingEvent};

#[tokio::test]
async fn pending_contact_replays_immediately_and_empty_replay_clears_tracking() -> Result {
	let Some(fixture) = fixture(false, -1).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let dest = Destination::Federation("pending.example".try_into()?);
	let Destination::Federation(server) = &dest else {
		unreachable!();
	};

	let old = enqueue(sending, &dest, SendingEvent::Pdu(pdu_id(1)));
	let mut futures = SendingFutures::new(); // sender state out-param
	let mut statuses = TransactionStatuses::new(); // sender state out-param
	let mut wakes = WakeQueue::new(); // sender state out-param

	sending.db.mark_as_active(once(&old));
	sending
		.startup_netburst(0, &mut futures, &mut statuses, &mut wakes)
		.await;

	assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Pending)));
	let stalled = sending
		.stalled
		.lock()
		.expect("locked")
		.get(server)
		.copied();

	assert_eq!(stalled, Some(None));

	assert!(!sending.notify_peer_alive(server).await);

	let admitted = sending.channels[0]
		.1
		.try_recv()
		.expect("pending flush");

	sending
		.handle_request(admitted, &mut futures, &mut statuses, &mut wakes)
		.await;

	assert_eq!(futures.len(), 1);
	assert!(
		!sending
			.stalled
			.lock()
			.expect("locked")
			.contains_key(server)
	);

	assert!(!sending.notify_peer_alive(server).await);
	sending.channels[0]
		.1
		.try_recv()
		.expect_err("no replay admitted");

	futures.clear();

	statuses.insert(dest.clone(), TransactionStatus::Pending);
	sending
		.stalled
		.lock()
		.expect("locked")
		.insert(server.clone(), None);

	sending.db.delete_active_request(&old.0);
	assert!(!sending.notify_peer_alive(server).await);

	let admitted = sending.channels[0]
		.1
		.try_recv()
		.expect("empty replay flush");

	sending
		.handle_request(admitted, &mut futures, &mut statuses, &mut wakes)
		.await;

	assert!(futures.is_empty());
	assert!(!statuses.contains_key(&dest));
	assert!(
		!sending
			.stalled
			.lock()
			.expect("locked")
			.contains_key(server)
	);

	assert!(!sending.notify_peer_alive(server).await);
	sending.channels[0]
		.1
		.try_recv()
		.expect_err("no replay admitted");

	Ok(())
}
