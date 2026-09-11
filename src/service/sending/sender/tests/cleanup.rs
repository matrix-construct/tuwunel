use std::iter::once;

use tuwunel_core::Result;

use super::{
	CurTransactionStatus, Destination, SendingEvent, SendingFutures, TransactionStatus, enqueue,
	fixture, pdu_id,
};

#[tokio::test]
async fn reused_destination_promotes_first_request_after_cleanup() -> Result {
	let Some(fixture) = fixture(false, -1).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let active = &sending.db.db["servercurrentevent_data"];
	let queued = &sending.db.db["servernameevent_data"];
	let destinations = [
		Destination::Appservice("cleanup".into()),
		Destination::Push("@u:localhost".try_into()?, "cleanup".into()),
	];

	for dest in destinations {
		let old = enqueue(sending, &dest, SendingEvent::Pdu(pdu_id(1)));
		let mut futures = SendingFutures::new();
		let mut statuses = CurTransactionStatus::new();

		sending.db.mark_as_active(once(&old));
		sending
			.startup_netburst(0, &mut futures, &mut statuses)
			.await;

		assert!(futures.is_empty());
		assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Pending)));

		match &dest {
			| Destination::Appservice(id) => sending.cleanup_events(Some(id), None, None).await,
			| Destination::Push(user_id, push_key) =>
				sending
					.cleanup_events(None, Some(user_id), Some(push_key))
					.await,
			| Destination::Federation(_) => unreachable!("fixture has no federation destination"),
		}?;

		assert!(
			active
				.get(&old.0)
				.await
				.is_err_and(|error| error.is_not_found())
		);

		let new_id = pdu_id(2);
		let successor = enqueue(sending, &dest, SendingEvent::Pdu(new_id));
		let events = sending
			.select_events(&dest, vec![successor.clone()], &mut statuses)
			.await?;

		assert_eq!(events, Some(vec![SendingEvent::Pdu(new_id)]));
		assert!(matches!(statuses.get(&dest), Some(TransactionStatus::Running)));
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
