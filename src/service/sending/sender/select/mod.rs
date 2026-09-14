mod device_changes;
mod presence;
mod receipts;

use std::{
	iter::once,
	sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use futures::{StreamExt, future::join3};
use ruma::{ServerName, api::federation::transactions::edu::Edu};
use tuwunel_core::{
	Result, implement, trace,
	utils::{BoolExt, ReadyExt},
};

use super::{EDU_LIMIT, NewEvents, RetryAction, TransactionStatus, TransactionStatuses};
use crate::{
	federation::ShouldAttempt,
	sending::{Destination, EduBuf, EduVec, SendingEvent, Service},
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
) -> Result<Option<Vec<SendingEvent>>> {
	let retry_action = if matches!(dest, Destination::Appservice(_))
		&& new_events
			.iter()
			.any(|(_, event)| matches!(event, SendingEvent::Flush))
	{
		RetryAction::Force
	} else {
		RetryAction::None
	};

	let (allow, retry) = self
		.select_events_current(dest, statuses, retry_action)
		.await;

	if !allow {
		return Ok(None);
	}

	if retry {
		let active: Vec<_> = self
			.db
			.active_requests_for(dest)
			.map(|(_, event)| event)
			.collect()
			.await;

		if !active.is_empty() {
			return Ok(Some(active));
		}
	}

	let _cork = self.db.db.cork();
	let events = self
		.db
		.retain_queued(new_events)
		.inspect(|item| self.db.mark_as_active(once(item)))
		.ready_filter_map(|(_, event)| {
			matches!(event, SendingEvent::Flush)
				.is_false()
				.then_some(event)
		})
		.collect()
		.await;

	Ok(Some(self.with_edus(dest, events).await))
}

#[implement(Service)]
async fn select_events_current(
	&self,
	dest: &Destination,
	statuses: &mut TransactionStatuses,
	retry_action: RetryAction,
) -> (bool, bool) {
	// peer_status gates federation only; appservice and push fall through.
	if let Destination::Federation(server) = dest
		&& let ShouldAttempt::No { .. } = self
			.services
			.federation
			.should_attempt(server)
			.await
	{
		return (false, false);
	}

	if let Some(status) = statuses.get_mut(dest) {
		return self.transition(dest, status, retry_action);
	}

	statuses.insert(dest.clone(), TransactionStatus::Running);
	(true, false)
}

/// Advance a destination's status for a new selection.
///
/// Returns whether selection may proceed and whether the active set must be
/// replayed first.
#[implement(Service)]
fn transition(
	&self,
	dest: &Destination,
	status: &mut TransactionStatus,
	retry_action: RetryAction,
) -> (bool, bool) {
	let remaining = self.push_backoff_remaining(Some(&*status));

	match status {
		| TransactionStatus::Running | TransactionStatus::RunningForceRetry => {
			if matches!(retry_action, RetryAction::Force) {
				*status = TransactionStatus::RunningForceRetry;
			}

			(false, false)
		},
		| TransactionStatus::Retrying { .. } if matches!(dest, Destination::Push(..)) =>
			(false, false),
		| TransactionStatus::Failed { tries, .. } => {
			let tries = *tries;

			trace!(?dest, tries, ?remaining, "Push destination remains in backoff");
			if remaining.is_some() {
				return (false, false);
			}

			*status = TransactionStatus::Retrying { tries };
			(true, true)
		},
		| TransactionStatus::Pending | TransactionStatus::Retrying { .. } => {
			*status = TransactionStatus::Running;
			(true, true)
		},
	}
}

/// Top up a federation transaction with the EDUs accrued since its last window.
///
/// Other destinations pass through unchanged.
#[implement(Service)]
pub(super) async fn with_edus(
	&self,
	dest: &Destination,
	mut events: Vec<SendingEvent>,
) -> Vec<SendingEvent> {
	let Destination::Federation(server_name) = dest else {
		return events;
	};

	let budget_used = events
		.iter()
		.filter(|event| matches!(event, SendingEvent::Edu(_)))
		.count();

	if let Ok(edus) = self.select_edus(server_name, budget_used).await {
		events.extend(edus.into_iter().map(SendingEvent::Edu));
	}

	events
}

#[implement(Service)]
#[tracing::instrument(name = "edus", level = "debug", skip_all)]
pub(super) async fn select_edus(
	&self,
	server_name: &ServerName,
	budget_used: usize,
) -> Result<EduVec> {
	let since = self.db.get_latest_educount(server_name).await;
	let since_upper = self.services.globals.current_count();

	// Nothing new since the last window: skip the scan and the watermark.
	if since == since_upper {
		return Ok(EduVec::new());
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
	if durable_len > 0 {
		self.db
			.persist_active_edus(server_name, &events[..durable_len]);
	}

	let last_count = max_edu_count.load(Ordering::Acquire);
	if last_count > since {
		self.db
			.set_latest_educount(server_name, last_count);
	}

	Ok(events)
}

/// Serialize one EDU into its inline queue buffer.
pub(super) fn edu_buf(edu: &Edu) -> EduBuf {
	let mut buf = EduBuf::new(); // serde_json::to_writer out-param

	serde_json::to_writer(&mut buf, edu).expect("EDU serializes to JSON");
	buf
}
