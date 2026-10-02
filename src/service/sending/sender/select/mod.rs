mod device_changes;
mod presence;
mod receipts;

use std::{
	iter::repeat_with,
	mem::replace,
	sync::atomic::{AtomicU64, AtomicUsize, Ordering},
	time::SystemTime,
};

use futures::{StreamExt, future::join3};
use ruma::{ServerName, api::federation::transactions::edu::Edu};
use tuwunel_core::{
	Result, implement, trace,
	utils::{BoolExt, ReadyExt},
};

use super::{
	DEQUEUE_LIMIT, EDU_LIMIT, NewEvents, RetryAction, TransactionStatus, TransactionStatuses,
	split::Split,
};
use crate::{
	federation::ShouldAttempt,
	sending::{
		Destination, EduBuf, EduVec, SendingEvent, Service,
		data::{Key, QueueItem},
	},
};

/// Output of one EDU selector.
///
/// `shipped` rides the current transaction up to the shared budget; `overflow`
/// past the budget is written as queued rows for later transactions to drain.
#[derive(Default)]
struct Selected {
	shipped: EduVec,
	overflow: Vec<EduBuf>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Selection {
	Events(Vec<QueueItem>),
	Slice(Vec<QueueItem>, Split),
	Busy,
	Refused {
		earliest_retry: SystemTime,
	},
}

enum Current {
	Ready {
		replay: bool,
	},
	Split(Split),
	Busy,
	Refused {
		earliest_retry: SystemTime,
	},
}

impl Selected {
	/// Take one EDU into the transaction while the shared budget lasts.
	///
	/// Once anything has overflowed every later EDU overflows too, so the
	/// shipped set is always a prefix of the selection order.
	fn push(&mut self, edu: EduBuf, events_len: &AtomicUsize) {
		if !self.overflow.is_empty() || events_len.fetch_add(1, Ordering::Relaxed) >= EDU_LIMIT {
			self.overflow.push(edu);
		} else {
			self.shipped.push(edu);
		}
	}
}

#[implement(Service)]
#[tracing::instrument(
	name = "select",
	level = "debug",
	skip_all,
	fields(
		?dest,
		new_events = %new_events.len(),
	),
)]
pub(super) async fn select_events(
	&self,
	dest: &Destination,
	new_events: NewEvents,
	statuses: &mut TransactionStatuses,
) -> Result<Selection> {
	let retry_action = if matches!(dest, Destination::Appservice(_))
		&& new_events
			.iter()
			.any(|(_, event)| matches!(event, SendingEvent::Flush))
	{
		RetryAction::Force
	} else {
		RetryAction::None
	};

	let current = self
		.select_events_current(dest, statuses, retry_action)
		.await;

	let retry = match current {
		| Current::Busy => return Ok(Selection::Busy),
		| Current::Refused { earliest_retry } =>
			return Ok(Selection::Refused { earliest_retry }),
		| Current::Ready { replay } => replay,
		| Current::Split(split) => match self.slice(dest, split).await {
			| Some((items, split)) => return Ok(Selection::Slice(items, split)),
			| None => true,
		},
	};

	if retry {
		let active: Vec<_> = self.db.active_requests_for(dest).collect().await;

		if !active.is_empty() {
			return Ok(Selection::Events(active));
		}
	}

	let _cork = self.db.db.cork();
	let items = self.claim_new(new_events).await;
	let items = self.with_edus(dest, items).await;

	Ok(Selection::Events(items))
}

#[implement(Service)]
async fn select_events_current(
	&self,
	dest: &Destination,
	statuses: &mut TransactionStatuses,
	retry_action: RetryAction,
) -> Current {
	// peer_status gates federation only; appservice and push fall through.
	if let Destination::Federation(server) = dest
		&& let ShouldAttempt::No { earliest_retry } = self
			.services
			.federation
			.should_attempt(server)
			.await
	{
		return Current::Refused { earliest_retry };
	}

	let Some(status) = statuses.get_mut(dest) else {
		statuses.insert(dest.clone(), TransactionStatus::Running { tries: 0 });
		return Current::Ready { replay: false };
	};

	let current = self.transition(dest, status, retry_action);

	if matches!(current, Current::Ready { replay: true } | Current::Split(_)) {
		self.clear_stalled(dest);
	}

	current
}

/// Advance a destination's status for a new selection.
///
/// Distinguishes busy destinations from permitted selection and active replay.
#[implement(Service)]
fn transition(
	&self,
	dest: &Destination,
	status: &mut TransactionStatus,
	retry_action: RetryAction,
) -> Current {
	let remaining = self.push_backoff_remaining(Some(&*status));

	match status {
		| TransactionStatus::Retrying { .. } if matches!(dest, Destination::Push(..)) =>
			Current::Busy,
		| TransactionStatus::Running { tries }
		| TransactionStatus::RunningForceRetry { tries } => {
			if matches!(retry_action, RetryAction::Force) {
				*status = TransactionStatus::RunningForceRetry { tries: *tries };
			}

			Current::Busy
		},
		| TransactionStatus::Failed { tries, .. } => {
			let tries = *tries;

			trace!(?dest, tries, ?remaining, "Push destination remains in backoff");
			if remaining.is_some() {
				return Current::Busy;
			}

			*status = TransactionStatus::Retrying { tries };
			Current::Ready { replay: true }
		},
		| TransactionStatus::Pending => {
			*status = TransactionStatus::Running { tries: 0 };
			Current::Ready { replay: true }
		},
		| TransactionStatus::Retrying { tries } => {
			*status = TransactionStatus::Running { tries: *tries };
			Current::Ready { replay: true }
		},
		| TransactionStatus::Splitting { tries, .. } => {
			let tries = *tries;

			launch(status, tries)
		},
	}
}

fn launch(status: &mut TransactionStatus, tries: u32) -> Current {
	match replace(status, TransactionStatus::Running { tries }) {
		| TransactionStatus::Splitting { split, .. } => Current::Split(split),
		| _ => Current::Ready { replay: true },
	}
}

#[implement(Service)]
fn clear_stalled(&self, dest: &Destination) {
	if let Destination::Federation(server) = dest {
		self.stalled
			.lock()
			.expect("locked")
			.remove(server);
	}
}

/// Claim a request's own queue rows.
///
/// Flush markers carry no row and are dropped; the claimed rows turn active
/// in one batch.
#[implement(Service)]
async fn claim_new(&self, new_events: NewEvents) -> Vec<QueueItem> {
	let items: Vec<_> = self
		.db
		.retain_queued(new_events)
		.ready_filter(|(_, event)| matches!(event, SendingEvent::Flush).is_false())
		.collect()
		.await;

	self.db.mark_as_active(items.iter());
	items
}

/// Top up a federation transaction with the EDUs accrued since its last window.
///
/// An empty transaction first claims the head of the queue; other destinations
/// pass through unchanged.
#[implement(Service)]
pub(super) async fn with_edus(
	&self,
	dest: &Destination,
	items: Vec<QueueItem>,
) -> Vec<QueueItem> {
	let Destination::Federation(server_name) = dest else {
		return items;
	};

	let items = if items.is_empty() {
		self.resume_queued(dest).await
	} else {
		items
	};

	// Fresh signing keys must not overtake an older queued signing update.
	if self
		.db
		.queued_requests(dest)
		.take(1)
		.count()
		.await
		.ne(&0)
	{
		return items;
	}

	let budget_used = items
		.iter()
		.filter(|(_, event)| matches!(event, SendingEvent::Edu(_)))
		.count();

	let edus = self.select_edus(server_name, budget_used).await;

	append_edus(items, edus)
}

fn append_edus(
	mut items: Vec<QueueItem>,
	edus: impl Iterator<Item = QueueItem>,
) -> Vec<QueueItem> {
	items.extend(edus);
	items
}

/// Claim the head of a destination's queue as its next transaction.
///
/// The claimed rows turn active in one batch.
#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
pub(super) async fn resume_queued(&self, dest: &Destination) -> Vec<QueueItem> {
	let queued: Vec<_> = self
		.db
		.queued_requests(dest)
		.take(DEQUEUE_LIMIT)
		.collect()
		.await;

	self.db.mark_as_active(queued.iter());
	queued
}

#[implement(Service)]
#[tracing::instrument(name = "edus", level = "debug", skip_all)]
pub(super) async fn select_edus(
	&self,
	server_name: &ServerName,
	budget_used: usize,
) -> impl Iterator<Item = QueueItem> {
	let since = self.db.get_latest_educount(server_name).await;
	let since_upper = self.services.globals.current_count();

	// Nothing new since the last window: skip the scan and the watermark.
	if since == since_upper {
		return keyed(Vec::new().into_iter(), EduVec::new());
	}

	let batch = (since, since_upper);

	debug_assert!(batch.0 <= batch.1, "since range must not be negative");

	let events_len = AtomicUsize::new(budget_used);
	let max_edu_count = AtomicU64::new(since);
	let device_changes =
		self.select_edus_device_changes(server_name, batch, &max_edu_count, &events_len);

	let receipts = self
		.server
		.config
		.allow_outgoing_read_receipts
		.then_async(|| {
			self.select_edus_receipts(server_name, batch, &max_edu_count, &events_len)
		});

	let presence = self
		.server
		.config
		.allow_outgoing_presence
		.then_async(|| {
			self.select_edus_presence(server_name, batch, &max_edu_count, &events_len)
		});

	let (device_changes, receipts, presence) = join3(device_changes, receipts, presence).await;
	let receipts = receipts.unwrap_or_default();

	// Presence rides last and is excluded from the durable prefix because
	// its content is compose-time-relative and regenerates fresh.
	let durable_len = device_changes
		.shipped
		.len()
		.saturating_add(receipts.shipped.len());

	let events: EduVec = device_changes
		.shipped
		.into_iter()
		.chain(receipts.shipped)
		.chain(presence.flatten())
		.collect();

	debug_assert!(budget_used.saturating_add(events.len()) <= EDU_LIMIT, "exceeded edus limit");

	// EDUs past the budget become queued rows drained by later transactions.
	let overflow: Vec<SendingEvent> = device_changes
		.overflow
		.into_iter()
		.chain(receipts.overflow)
		.map(SendingEvent::Edu)
		.collect();

	if !overflow.is_empty() {
		let dest = Destination::Federation(server_name.to_owned());

		self.db
			.queue_requests(overflow.iter().map(|event| (event, &dest)));
	}

	// Persist the durable prefix so a failed or restarted transaction
	// replays it; the ACK deletes these active rows.
	let keys = self
		.db
		.persist_active_edus(server_name, &events[..durable_len]);

	let last_count = max_edu_count.load(Ordering::Acquire);
	if last_count > since {
		self.db
			.set_latest_educount(server_name, last_count);
	}

	keyed(keys, events)
}

/// Pair composed EDUs with their durable keys.
///
/// Presence rides past the durable prefix with empty keys, so a delivery has no
/// row of it to acknowledge.
fn keyed(keys: impl Iterator<Item = Key>, edus: EduVec) -> impl Iterator<Item = QueueItem> {
	keys.chain(repeat_with(Key::new))
		.zip(edus)
		.map(|(key, edu)| (key, SendingEvent::Edu(edu)))
}

/// Serialize one EDU into its inline queue buffer.
pub(super) fn edu_buf(edu: &Edu) -> EduBuf {
	let mut buf = EduBuf::new(); // serde_json::to_writer out-param

	serde_json::to_writer(&mut buf, edu).expect("EDU serializes to JSON");
	buf
}
