use std::time::Instant;

use futures::StreamExt;
use tuwunel_core::{Error, debug, implement};

use super::{
	DEQUEUE_LIMIT, NewEvents, RetryAction, SendingFutures, TransactionStatus,
	TransactionStatuses, WakeQueue, dispatch::SendingResult,
};
use crate::sending::{Destination, Service};

#[implement(Service)]
#[tracing::instrument(name = "response", level = "debug", skip_all)]
pub(super) async fn handle_response<'a>(
	&'a self,
	response: SendingResult,
	futures: &mut SendingFutures<'a>,
	statuses: &mut TransactionStatuses,
	wakes: &mut WakeQueue,
) {
	match response {
		| Ok(dest) =>
			self.handle_response_ok(dest, futures, statuses)
				.await,
		| Err((dest, error)) =>
			self.handle_response_err(dest, error, futures, statuses, wakes)
				.await,
	}
}

#[implement(Service)]
#[expect(clippy::needless_pass_by_ref_mut)]
pub(super) async fn handle_response_ok<'a>(
	&'a self,
	dest: Destination,
	futures: &mut SendingFutures<'a>,
	statuses: &mut TransactionStatuses,
) {
	let _cork = self.db.db.cork();

	self.db
		.delete_all_active_requests_for(&dest)
		.await;

	let new_events: NewEvents = self
		.db
		.queued_requests(&dest)
		.take(DEQUEUE_LIMIT)
		.collect()
		.await;

	if !new_events.is_empty() {
		self.db.mark_as_active(new_events.iter());
	}

	let events = new_events
		.into_iter()
		.map(|(_, event)| event)
		.collect();

	let events = self.with_edus(&dest, events).await;
	if events.is_empty() {
		statuses.remove(&dest);
		return;
	}

	run_status(&dest, statuses);
	futures.push(self.send_events(dest, events));
}

#[implement(Service)]
async fn handle_response_err<'a>(
	&'a self,
	dest: Destination,
	error: Error,
	futures: &mut SendingFutures<'a>,
	statuses: &mut TransactionStatuses,
	wakes: &mut WakeQueue,
) {
	debug!(?dest, ?error, "Transaction failed");
	let retry_action = fail_status(&dest, statuses);

	match dest {
		| Destination::Federation(server) => self.arm_federation_wake(server, wakes).await,
		| dest @ Destination::Push(..) => self.arm_push_wake(dest, &error, statuses, wakes),
		| dest if matches!(retry_action, RetryAction::Force) =>
			self.handle_force_retry(dest, futures, statuses)
				.await,
		| _ => {},
	}
}

/// Mark a destination's transaction as running, if one is tracked.
fn run_status(dest: &Destination, statuses: &mut TransactionStatuses) {
	if let Some(status) = statuses.get_mut(dest) {
		*status = TransactionStatus::Running;
	}
}

/// Advance a destination's status after a failed transaction.
///
/// Reports whether a forced retry was requested while the transaction ran.
fn fail_status(dest: &Destination, statuses: &mut TransactionStatuses) -> RetryAction {
	// Push backs off locally; federation defers to peer_status, appservice retries.
	let push = matches!(dest, Destination::Push(..));

	let Some(status) = statuses.get_mut(dest) else {
		return RetryAction::None;
	};

	let (tries, retry_action) = match status {
		| TransactionStatus::Pending | TransactionStatus::Running => (1, RetryAction::None),
		| TransactionStatus::RunningForceRetry => (1, RetryAction::Force),
		| TransactionStatus::Failed { tries, .. } | TransactionStatus::Retrying { tries } =>
			(tries.saturating_add(1), RetryAction::None),
	};

	*status = if push {
		TransactionStatus::Failed { tries, last: Instant::now() }
	} else {
		TransactionStatus::Retrying { tries }
	};

	retry_action
}

#[implement(Service)]
pub(super) async fn handle_force_retry<'a>(
	&'a self,
	dest: Destination,
	futures: &mut SendingFutures<'a>,
	statuses: &mut TransactionStatuses,
) {
	let Ok(Some(events)) = self
		.select_events(&dest, NewEvents::new(), statuses)
		.await
	else {
		return;
	};

	self.schedule_events(dest, events, futures, statuses);
}
