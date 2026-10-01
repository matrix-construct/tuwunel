use futures::StreamExt;
use tuwunel_core::{Result, utils::IterStream};

use super::{
	SendingFutures, TransactionStatuses, WakeQueue, config, destination, enqueue, fixture, pdu_id,
};
use crate::sending::{Destination, SendingEvent};

#[tokio::test]
async fn startup_discovery_arms_each_destination_only_on_its_worker() -> Result {
	let config = config(false, 0).merge(("sender_workers", 2));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let count = 32;
	let owns = |dest: &Destination, id: usize| {
		matches!(dest, Destination::Federation(server)
			if sending.federation_shard_id(server) == id && sending.shard_id(dest) == id)
	};

	{
		let _cork = sending.db.db.cork();

		for index in 0..count {
			let name = format!("shard{index:05}.example");
			let dest = destination(&name);

			enqueue(sending, &dest, SendingEvent::Pdu(pdu_id(1)));
		}
	}

	let (armed, ..) = (0..sending.channels.len())
		.stream()
		.fold((0, sending, owns), async move |(total, sending, owns), id| {
			let mut futures = SendingFutures::new(); // startup state out-param
			let mut statuses = TransactionStatuses::new(); // startup state out-param
			let mut wakes = WakeQueue::new(); // startup state out-param

			sending
				.startup_netburst(id, &mut futures, &mut statuses, &mut wakes)
				.await;

			assert!(futures.is_empty());
			assert!(statuses.is_empty());
			let owned = wakes.iter().all(|wake| owns(&wake.0.1, id));

			assert!(owned);

			(total + wakes.len(), sending, owns)
		})
		.await;

	assert_eq!(armed, count);

	let mut futures = SendingFutures::new(); // startup state out-param
	let mut statuses = TransactionStatuses::new(); // startup state out-param
	let mut wakes = WakeQueue::new(); // startup state out-param

	sending
		.startup_netburst(sending.channels.len(), &mut futures, &mut statuses, &mut wakes)
		.await;

	assert!(futures.is_empty());
	assert!(statuses.is_empty());
	assert!(wakes.is_empty());

	Ok(())
}
