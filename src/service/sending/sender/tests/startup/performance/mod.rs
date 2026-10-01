use std::{
	cmp::Reverse,
	sync::atomic::{AtomicUsize, Ordering},
	time::Instant,
};

use tokio::time::Instant as TokioInstant;
use tuwunel_core::{Result, format_small_string};
use tuwunel_database::Txn;

use super::{
	SendingFutures, TransactionStatus, TransactionStatuses, WakeQueue, config, destination,
	fixture, pdu_id,
};
use crate::{
	resolver::DestString,
	sending::{Destination, Service, data::Key, dest::DestinationRef},
};

#[tokio::test]
async fn real_boot_cost_at_representative_destination_scale() -> Result {
	let Some(fixture) = fixture(config(false, 0)).await? else {
		return Ok(());
	};

	let sending = &fixture.services.sending;
	let destinations = 1_800;
	let queued = &sending.db.db["servernameevent_data"];

	for depth in [1, 64] {
		Txn::insert(queued, rows(destinations, depth)).execute();

		let mut futures = SendingFutures::new(); // startup state out-param
		let statuses = TransactionStatuses::new();
		let wakes = WakeQueue::new();
		let started = Instant::now();
		let (statuses, wakes) = startup(sending, &mut futures, statuses, wakes).await;
		let elapsed = started.elapsed();
		let comparisons = destinations * (destinations - 1) / 2;

		assert_eq!(wakes.len(), destinations);
		assert!(futures.is_empty());
		assert!(statuses.is_empty());
		eprintln!(
			"real boot destinations={destinations} depth={depth} rows={} elapsed={elapsed:?} \
			 source_derived_heap_comparisons={comparisons}",
			destinations * depth,
		);
	}

	eprintln!(
		"boot sizes cursor={} borrowed_destination={} owned_destination={} status={} \
		 heap_entry={}",
		size_of::<Vec<u8>>(),
		size_of::<DestinationRef<'_>>(),
		size_of::<Destination>(),
		size_of::<TransactionStatus>(),
		size_of::<Reverse<(TokioInstant, Destination)>>(),
	);

	Ok(())
}

fn rows(destinations: usize, depth: usize) -> impl Iterator<Item = (Key, &'static [u8])> {
	(0..destinations).flat_map(move |index| {
		let name = name(index);
		let dest = destination(&name);

		(0..depth).map(move |row| {
			let count = u64::try_from(row).expect("row count");
			let key = dest.event_key(&pdu_id(count));

			(key, b"".as_slice())
		})
	})
}

fn name(index: usize) -> DestString { format_small_string!("scale{index:05}.example") }

#[tracing::instrument(level = "trace", skip_all)]
async fn startup<'a>(
	sending: &'a Service,
	futures: &mut SendingFutures<'a>,
	mut statuses: TransactionStatuses,
	mut wakes: WakeQueue,
) -> (TransactionStatuses, WakeQueue) {
	sending
		.startup_netburst(0, futures, &mut statuses, &mut wakes)
		.await;

	(statuses, wakes)
}

#[test]
fn aggregate_boot_heap_comparisons_are_measured_separately() {
	for destinations in [100, 1_800] {
		let comparisons = AtomicUsize::new(0);
		let count = |_: &&Reverse<(TokioInstant, Destination)>| {
			comparisons.fetch_add(1, Ordering::Relaxed);
		};

		let due = TokioInstant::now();
		let started = Instant::now();
		let wakes = (0..destinations)
			.map(|index| {
				let name = name(index);

				destination(&name)
			})
			.fold(WakeQueue::new(), |mut wakes, dest| {
				let armed = wakes
					.iter()
					.inspect(count)
					.any(|Reverse((_, armed))| armed == &dest);

				assert!(!armed);
				wakes.push(Reverse((due, dest)));
				wakes
			});

		let elapsed = started.elapsed();
		let expected = destinations * (destinations - 1) / 2;

		assert_eq!(wakes.len(), destinations);
		assert_eq!(comparisons.load(Ordering::Relaxed), expected);
		eprintln!(
			"boot heap destinations={destinations} measured_comparisons={expected} \
			 elapsed={elapsed:?}",
		);
	}
}
