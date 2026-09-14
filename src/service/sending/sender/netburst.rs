use std::collections::BTreeMap;

use futures::StreamExt;
use tuwunel_core::{implement, itertools::Itertools, utils::ReadyExt, warn};

use super::{SendingFutures, TransactionStatus, TransactionStatuses};
use crate::sending::{Destination, Msg, SendingEvent, Service};

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
				| true => TransactionStatus::Running,
				| false => TransactionStatus::Pending,
			};

			statuses.insert(dest.clone(), status);
			if netburst {
				futures.push(self.send_events(dest, events));
			}
		});

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

		self.handle_request(msg, futures, statuses).await;
	}
}
