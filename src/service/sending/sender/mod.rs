mod dispatch;
mod netburst;
mod response;
mod select;
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
};

use self::dispatch::SendingFuture;
use super::{Destination, Msg, SendingEvent, Service, data::QueueItem};

/// In-flight bookkeeping for one `Destination`.
///
/// Cross-attempt backoff lives in `peer_status` (federation only); appservice
/// and push paths keep their own status because they are not server-keyed.
#[derive(Debug)]
enum TransactionStatus {
	/// A durable active generation awaiting its first dispatch after restart.
	Pending,
	Running,
	RunningForceRetry,
	/// Push backoff: the attempt count and the time of the last failure.
	Failed {
		tries: u32,
		last: Instant,
	},
	/// A retry is in flight after this many failures.
	Retrying {
		tries: u32,
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
/// Every recorded federation or push failure arms an entry. Stale entries are
/// consumed by the destination's in-flight or newer failure generation. The
/// heap is bounded by concurrently failing destinations, transient federation
/// re-arms, and stale push entries.
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

	self.startup_netburst(id, &mut futures, &mut statuses)
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
				Ok(request) => self.handle_request(request, futures, statuses).await,
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

	if let Ok(Some(events)) = self
		.select_events(&msg.dest, new_events, statuses)
		.await
	{
		self.schedule_events(msg.dest, events, futures, statuses);
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
	events: Vec<SendingEvent>,
	futures: &mut SendingFutures<'a>,
	statuses: &mut TransactionStatuses,
) {
	if events.is_empty() {
		statuses.remove(&dest);
	} else {
		futures.push(self.send_events(dest, events));
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
				Some(Ok(dest)) => self.db.delete_all_active_requests_for(&dest).await,
				Some(_) => {},
				None => return,
			},
		}
	}
}
