use std::time::Instant;

use futures::StreamExt;
use tuwunel_core::{Error, debug, error::error_chain, implement, info, warn};

use super::{
	DEQUEUE_LIMIT, NewEvents, RetryAction, SendingFutures, TransactionStatus,
	TransactionStatuses, WakeQueue, dispatch::SendingResult, select::Selection,
};
use crate::{
	federation::is_content_rejection,
	sending::{Destination, Service},
};

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
	log_recovery(&dest, statuses);

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

/// Log a delivery that ends a destination's streak of failed transactions.
///
/// Reads the streak before `run_status` resets it, so it runs first. Push,
/// whose retry in flight is `Retrying`, reports its own failures instead.
fn log_recovery(dest: &Destination, statuses: &TransactionStatuses) {
	if let Some(
		&(TransactionStatus::Running { tries } | TransactionStatus::RunningForceRetry { tries }),
	) = statuses.get(dest)
		&& tries > 0
	{
		info!(?dest, streak = tries, "Transaction delivered after failures");
	}
}

/// Mark a destination's transaction as running, if one is tracked.
fn run_status(dest: &Destination, statuses: &mut TransactionStatuses) {
	if let Some(status) = statuses.get_mut(dest) {
		*status = TransactionStatus::Running { tries: 0 };
	}
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
	let retry_action = fail_status(&dest, statuses);

	log_failure(&dest, &error, statuses);

	let tries = match statuses.get(&dest) {
		| Some(TransactionStatus::Retrying { tries }) => *tries,
		| _ => 0,
	};

	match dest {
		| dest @ Destination::Push(..) => self.arm_push_wake(dest, &error, statuses, wakes),
		| Destination::Federation(server) => {
			if tries > 0 {
				self.stalled
					.lock()
					.expect("locked")
					.insert(server.clone(), Some(Instant::now()));
			}

			self.arm_federation_wake(server, tries, wakes)
				.await;
		},
		| dest if matches!(retry_action, RetryAction::Force) =>
			self.handle_force_retry(dest, futures, statuses)
				.await,
		| _ => {},
	}
}

/// Advance a destination's status after a failed transaction.
///
/// Reports whether a forced retry was requested while the transaction ran.
fn fail_status(dest: &Destination, statuses: &mut TransactionStatuses) -> RetryAction {
	// Push records its local clock; other destinations wait for a replay trigger.
	let push = matches!(dest, Destination::Push(..));

	let Some(status) = statuses.get_mut(dest) else {
		return RetryAction::None;
	};

	let (tries, retry_action) = match status {
		| TransactionStatus::Pending => (1, RetryAction::None),
		| TransactionStatus::RunningForceRetry { tries } =>
			(tries.saturating_add(1), RetryAction::Force),
		| TransactionStatus::Running { tries }
		| TransactionStatus::Failed { tries, .. }
		| TransactionStatus::Retrying { tries } => (tries.saturating_add(1), RetryAction::None),
	};

	*status = if push {
		TransactionStatus::Failed { tries, last: Instant::now() }
	} else {
		TransactionStatus::Retrying { tries }
	};

	retry_action
}

/// Log a failed transaction, at warn when a content rejection starts a
/// destination's streak.
///
/// A content rejection records no peer backoff and its batch replays unchanged,
/// so without the warning a stuck destination is silent at default levels.
/// Call it after `fail_status`, whose advanced status it reads.
fn log_failure(dest: &Destination, error: &Error, statuses: &TransactionStatuses) {
	match statuses.get(dest) {
		| Some(TransactionStatus::Retrying { tries: 1 }) if is_content_rejection(error) =>
			warn!(?dest, chain = %error_chain(error), "Transaction failed"),
		| _ => debug!(?dest, chain = %error_chain(error), "Transaction failed"),
	}
}

#[implement(Service)]
pub(super) async fn handle_force_retry<'a>(
	&'a self,
	dest: Destination,
	futures: &mut SendingFutures<'a>,
	statuses: &mut TransactionStatuses,
) {
	let Ok(Selection::Events(events)) = self
		.select_events(&dest, NewEvents::new(), statuses)
		.await
	else {
		return;
	};

	self.schedule_events(dest, events, futures, statuses);
}
