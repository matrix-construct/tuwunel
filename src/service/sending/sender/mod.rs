mod dispatch;
mod netburst;
mod response;
mod select;
mod split;
#[cfg(test)]
mod tests;
mod wake;

use std::{
	cmp::Reverse,
	collections::{BinaryHeap, HashMap},
	sync::Arc,
	time::{Duration, Instant},
};

use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use tokio::{
	select,
	time::{Instant as TokioInstant, sleep_until},
};
use tuwunel_core::{
	Result, implement,
	smallvec::{SmallVec, smallvec},
	trace,
	utils::BoolExt,
};

use self::{
	dispatch::{Completion, SendingFuture},
	select::Selection,
	split::Split,
	wake::{arm_park_wake, arm_wake, is_armed},
};
use super::{Destination, Msg, SendingEvent, Service, data::QueueItem};

/// In-flight bookkeeping for one `Destination`.
///
/// Federation uses the peer gate when refused and the sender curve otherwise.
/// Appservice and push retain their own destination status across retries.
#[derive(Debug)]
enum TransactionStatus {
	/// A durable active generation awaiting its first dispatch after restart.
	Pending,

	/// A transaction is in flight after this many consecutive failures.
	Running {
		tries: u32,
	},

	/// As `Running`, with a forced retry requested while it runs.
	RunningForceRetry {
		tries: u32,
	},

	/// Push backoff: consecutive failures and the time of the last one.
	Failed {
		tries: u32,
		last: Instant,
	},

	/// Retry state after this many consecutive failures.
	///
	/// For push the retry is already in flight; for other destinations the
	/// batch waits for its replay.
	Retrying {
		tries: u32,
	},

	/// As `Retrying`, while a rejected transaction's rooms are sent apart.
	Splitting {
		tries: u32,
		split: Split,
	},
}

#[derive(Clone, Copy)]
enum RetryAction {
	None,
	Force,
}

type SendingFutures<'a> = FuturesUnordered<SendingFuture<'a>>;
type TransactionStatuses = HashMap<Destination, TransactionStatus>;

/// The queue items one request brings to a selection.
///
/// A request carries one item; a badge wake dequeues up to `DEQUEUE_LIMIT`.
type NewEvents = SmallVec<[QueueItem; 1]>;

/// Per-worker retry timer keyed by earliest-retry deadline and destination.
///
/// Every federation, appservice, or push failure arms an entry. Traffic-triggered retries
/// can leave stale entries, which skip an in-flight transaction or replay a
/// waiting one. Multiple entries may therefore remain for one destination.
type WakeQueue = BinaryHeap<Reverse<(TokioInstant, Destination)>>;

const DEQUEUE_LIMIT: usize = 48;

/// Most PDUs one federation transaction may carry.
///
/// The spec caps a `/send` body at this many PDUs; inbound bodies past it are
/// rejected and outbound composition stays under it.
pub const PDU_LIMIT: usize = 50;

/// Most EDUs one federation transaction may carry.
///
/// The spec caps a `/send` body at this many EDUs; inbound bodies past it are
/// rejected and outbound composition stays under it.
pub const EDU_LIMIT: usize = 100;

#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub(super) async fn sender(self: Arc<Self>, id: usize) -> Result {
	// The worker's state, threaded as &mut through every phase.
	let mut statuses = TransactionStatuses::new();
	let mut futures = SendingFutures::new();
	let mut wakes = WakeQueue::new();

	self.startup_netburst(id, &mut futures, &mut statuses, &mut wakes)
		.boxed() // size firewall
		.await;

	self.work_loop(id, &mut futures, &mut statuses, &mut wakes)
		.await;

	if !futures.is_empty() {
		self.finish_responses(&mut futures)
			.boxed() // size firewall
			.await;
	}

	Ok(())
}

#[implement(Service)]
#[tracing::instrument(
	name = "work",
	level = "trace",
	skip_all,
	fields(
		futures = %futures.len(),
		statuses = %statuses.len(),
	),
)]
async fn work_loop<'a>(
	&'a self,
	id: usize,
	futures: &mut SendingFutures<'a>,
	statuses: &mut TransactionStatuses,
	wakes: &mut WakeQueue,
) {
	let receiver = &self
		.channels
		.get(id)
		.expect("Missing channel for sender worker")
		.1;

	while !receiver.is_closed() {
		let next_due = wakes
			.peek()
			.map_or_else(TokioInstant::now, |Reverse((instant, _))| *instant);

		select! {
			Some(response) = futures.next() => {
				self.handle_response(response, futures, statuses, wakes).await;
			},
			request = receiver.recv_async() => match request {
				Ok(request) => self.handle_request(request, futures, statuses, wakes).await,
				Err(_) => return,
			},
			() = sleep_until(next_due), if !wakes.is_empty() => {
				self.drain_due_wakes(futures, statuses, wakes).await;
			},
		}
	}
}

#[implement(Service)]
#[tracing::instrument(name = "request", level = "debug", skip_all)]
async fn handle_request<'a>(
	&'a self,
	msg: Msg,
	futures: &mut SendingFutures<'a>,
	statuses: &mut TransactionStatuses,
	wakes: &mut WakeQueue,
) {
	let synthetic_badge =
		msg.queue_id.is_empty() && matches!(&msg.event, SendingEvent::BadgeRefresh);

	let new_events = match (synthetic_badge, statuses.contains_key(&msg.dest)) {
		| (false, _) => smallvec![(msg.queue_id, msg.event)],
		| (true, true) => NewEvents::new(),
		| (true, false) =>
			self.db
				.queued_requests(&msg.dest)
				.take(DEQUEUE_LIMIT)
				.collect()
				.await,
	};

	if let Ok(selection) = self
		.select_events(&msg.dest, new_events, statuses)
		.await
	{
		self.schedule_events(msg.dest, selection, futures, statuses, wakes);
	}
}

#[implement(Service)]
#[expect(
	clippy::needless_pass_by_ref_mut,
	reason = "mutable reference avoids requiring SendingFutures to be Sync"
)]
fn schedule_events<'a>(
	&'a self,
	dest: Destination,
	selection: Selection,
	futures: &mut SendingFutures<'a>,
	statuses: &mut TransactionStatuses,
	wakes: &mut WakeQueue,
) {
	match selection {
		| Selection::Events(items) if items.is_empty() => {
			statuses.remove(&dest);
		},
		| Selection::Events(items) => futures.push(self.send_events(dest, items, None)),
		| Selection::Slice(items, split) =>
			futures.push(self.send_events(dest, items, Some(split))),
		| Selection::Parked { until } => {
			statuses.remove(&dest);
			arm_park_wake(wakes, dest, until);
		},
		| Selection::Refused { earliest_retry } if is_armed(wakes, &dest).is_false() =>
			arm_wake(wakes, dest, earliest_retry),
		| Selection::Refused { .. } | Selection::Busy => {},
	}
}

#[implement(Service)]
#[tracing::instrument(
	name = "finish",
	level = "info",
	skip_all,
	fields(
		futures = %futures.len(),
	),
)]
async fn finish_responses<'a>(&'a self, futures: &mut SendingFutures<'a>) {
	let timeout = Duration::from_secs(self.server.config.sender_shutdown_timeout);
	let now = TokioInstant::now();
	let deadline = now.checked_add(timeout).unwrap_or(now);

	loop {
		trace!(remaining = futures.len(), "Waiting for requests to complete");
		select! {
			() = sleep_until(deadline) => return,
			response = futures.next() => match response {
				Some(Completion { result: Ok(_), keys, .. }) =>
					self.db.delete_active_requests(&keys),
				Some(_) => {},
				None => return,
			},
		}
	}
}
