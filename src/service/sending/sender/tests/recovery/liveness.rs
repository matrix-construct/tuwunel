use std::{iter::once, time::Duration};

use http::StatusCode;
use ruma::{OwnedServerName, api::error::ErrorBody};
use serde_json::Value;
use tuwunel_core::{Error, Result};

use super::{
	SendingFutures, TransactionStatus, TransactionStatuses, WakeQueue, enqueue, fixture, pdu_id,
};
use crate::{
	federation::Classification,
	sending::{Destination, Msg, SendingEvent},
};

#[tokio::test]
async fn inbound_admission_and_delayed_consumption_preserve_stalled_lifecycle() -> Result {
	let Some(fixture) = fixture(false, -1).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let server: OwnedServerName = "liveness.example".try_into()?;
	let dest = Destination::Federation(server.clone());
	let receiver = &sending.channels[0].1;
	let old = enqueue(sending, &dest, SendingEvent::Pdu(pdu_id(1)));
	let mut futures = SendingFutures::new(); // sender state out-param
	let mut statuses: TransactionStatuses =
		[(dest.clone(), TransactionStatus::Running { tries: 0 })].into();

	let mut wakes = WakeQueue::new(); // sender state out-param

	assert!(!sending.notify_peer_alive(&server).await);
	receiver
		.try_recv()
		.expect_err("no replay admitted");

	sending.db.mark_as_active(once(&old));

	let response = || {
		let rejection = ErrorBody::Json(Value::Null).into_error(StatusCode::FORBIDDEN);

		Err((dest.clone(), Error::Federation(server.clone(), rejection)))
	};

	sending
		.handle_response(response(), &mut futures, &mut statuses, &mut wakes)
		.await;

	let last = sending.stalled.lock().expect("locked")[&server].expect("failure clock");

	assert!(!sending.notify_peer_alive(&server).await);
	receiver
		.try_recv()
		.expect_err("no replay admitted");

	let aged = last
		.checked_sub(Duration::from_secs(sending.server.config.sender_timeout))
		.expect("failure floor");

	sending
		.stalled
		.lock()
		.expect("locked")
		.insert(server.clone(), Some(aged));

	assert!(!sending.notify_peer_alive(&server).await);

	let admitted = receiver
		.try_recv()
		.expect("stalled flush admitted");

	assert_eq!(admitted.dest, dest);
	assert_eq!(admitted.event, SendingEvent::Flush);

	sending
		.handle_response(response(), &mut futures, &mut statuses, &mut wakes)
		.await;

	let refreshed = sending.stalled.lock().expect("locked")[&server].expect("refreshed clock");

	assert!(refreshed >= last);
	assert!(!sending.notify_peer_alive(&server).await);
	receiver
		.try_recv()
		.expect_err("no replay admitted");

	sending
		.handle_request(admitted, &mut futures, &mut statuses, &mut wakes)
		.await;

	assert_eq!(futures.len(), 1);
	assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Running { tries: 2 })));
	assert!(
		!sending
			.stalled
			.lock()
			.expect("locked")
			.contains_key(&server)
	);

	futures.clear();
	sending
		.handle_response(response(), &mut futures, &mut statuses, &mut wakes)
		.await;

	fixture
		.services
		.federation
		.record_failure(&server, Classification::Transient);

	assert!(sending.notify_peer_alive(&server).await);
	receiver.try_recv().expect("peer reset flush");

	let msg = Msg {
		dest: dest.clone(),
		event: SendingEvent::Flush,
		queue_id: Vec::new(),
	};

	sending
		.handle_request(msg, &mut futures, &mut statuses, &mut wakes)
		.await;

	assert!(
		!sending
			.stalled
			.lock()
			.expect("locked")
			.contains_key(&server)
	);

	Ok(())
}
