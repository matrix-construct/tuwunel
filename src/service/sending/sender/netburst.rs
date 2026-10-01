use std::{cmp::Reverse, collections::BTreeMap, time::Duration};

use futures::StreamExt;
use tuwunel_core::{
	implement,
	itertools::Itertools,
	utils::{IterStream, ReadyExt, rand::secs as rand_secs, stream::TryIgnore},
	warn,
};

use super::{
	SendingFutures, TransactionStatus, TransactionStatuses, WakeQueue,
	wake::{arm_wake, arm_wake_in},
};
use crate::{
	federation::ShouldAttempt,
	sending::{Destination, Msg, SendingEvent, Service},
};

const BOOT_ARM_PACE: Duration = Duration::from_secs(2);

#[implement(Service)]
#[tracing::instrument(
	name = "netburst",
	level = "debug",
	skip_all,
	fields(
		futures = %futures.len(),
	),
)]
pub(super) async fn startup_netburst<'a>(
	&'a self,
	id: usize,
	futures: &mut SendingFutures<'a>,
	statuses: &mut TransactionStatuses,
	wakes: &mut WakeQueue,
) {
	let netburst = self.server.config.startup_netburst;
	let keep = usize::try_from(self.server.config.startup_netburst_keep).ok();
	let txns = self
		.db
		.active_requests()
		.ready_filter(|(_, _, dest)| self.shard_id(dest) == id)
		.ready_fold(
			BTreeMap::new(),
			|mut txns: BTreeMap<Destination, Vec<SendingEvent>>, (key, event, dest)| {
				let len = txns.get(&dest).map_or(0, Vec::len);

				match keep {
					| Some(limit) if len >= limit => {
						warn!(?dest, key = %String::from_utf8_lossy(&key), "Dropping unsent event");
						self.db.delete_active_request(&key);
					},
					| _ => txns.entry(dest).or_default().push(event),
				}

				txns
			},
		)
		.await;

	txns.into_iter()
		.filter(|(_, events)| !events.is_empty())
		.for_each(|(dest, events)| {
			let status = match netburst {
				| true => TransactionStatus::Running { tries: 0 },
				| false => TransactionStatus::Pending,
			};

			statuses.insert(dest.clone(), status);
			if !netburst {
				self.mark_pending(&dest);
			}

			if netburst {
				futures.push(self.send_events(dest, events));
			}
		});

	if !self.server.config.maintenance {
		self.arm_inherited_destinations(id, statuses, wakes)
			.await;
	}

	// Active transaction generations must own their queued successors before
	// queued-only badge destinations are woken.
	if !netburst || keep == Some(0) {
		return;
	}

	let destinations: Vec<_> = self
		.db
		.queued_badge_refresh_destinations()
		.ready_filter(|dest| self.shard_id(dest) == id)
		.collect()
		.await;

	for dest in destinations.into_iter().sorted_unstable().dedup() {
		let msg = Msg {
			dest,
			event: SendingEvent::BadgeRefresh,
			queue_id: Vec::new(),
		};

		self.handle_request(msg, futures, statuses, wakes)
			.await;
	}
}

#[implement(Service)]
fn mark_pending(&self, dest: &Destination) {
	if let Destination::Federation(server) = dest {
		self.stalled
			.lock()
			.expect("locked")
			.insert(server.clone(), None);
	}
}

#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
async fn arm_inherited_destinations(
	&self,
	id: usize,
	statuses: &TransactionStatuses,
	wakes: &mut WakeQueue,
) {
	let pending = statuses
		.iter()
		.filter(|(_, status)| matches!(status, TransactionStatus::Pending))
		.filter(|(dest, _)| matches!(dest, Destination::Federation(_)))
		.map(|(dest, _)| dest.clone())
		.stream();

	let queued = self
		.db
		.queued_federation_destinations(|server| self.federation_shard_id(server) == id)
		.inspect(|result| {
			if let Err(error) = result {
				warn!(%error, "Queued federation discovery failed");
			}
		})
		.ignore_err()
		.ready_filter(|dest| !statuses.contains_key(dest));

	pending
		.chain(queued)
		.fold((0_u64, wakes), async move |(index, wakes), dest| {
			let index = self.arm_startup_wake(dest, index, wakes).await;

			(index, wakes)
		})
		.await;
}

#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
pub(super) async fn arm_startup_wake(
	&self,
	dest: Destination,
	index: u64,
	wakes: &mut WakeQueue,
) -> u64 {
	if wakes
		.iter()
		.any(|Reverse((_, armed))| armed == &dest)
	{
		return index;
	}

	let Destination::Federation(server) = &dest else {
		return index;
	};

	match self
		.services
		.federation
		.should_attempt(server)
		.await
	{
		| ShouldAttempt::No { earliest_retry } => {
			arm_wake(wakes, dest, earliest_retry);
			index
		},
		| ShouldAttempt::Yes | ShouldAttempt::Deprioritize => {
			let delay = boot_delay(self.server.config.sender_timeout, index);

			arm_wake_in(wakes, dest, delay);
			index.saturating_add(1)
		},
	}
}

fn boot_delay(timeout: u64, index: u64) -> Duration {
	let offset = match timeout {
		| 0 => Duration::ZERO,
		| _ => rand_secs(0..timeout),
	};

	let paced = Duration::from_secs(index.saturating_mul(BOOT_ARM_PACE.as_secs()));

	offset.saturating_add(paced)
}
