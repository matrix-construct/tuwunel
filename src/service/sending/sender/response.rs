use std::time::Instant;

use tuwunel_core::{Error, debug, error::error_chain, implement, info, warn};

use super::{
	NewEvents, RetryAction, SendingFutures, TransactionStatus, TransactionStatuses, WakeQueue,
	dispatch::{Completion, SendingError},
	select::Selection,
	split::Split,
	wake::arm_appservice_wake,
};
use crate::{
	federation::is_content_rejection,
	sending::{Destination, Service},
};

#[implement(Service)]
#[tracing::instrument(name = "response", level = "debug", skip_all)]
pub(super) async fn handle_response<'a>(
	&'a self,
	Completion { result, keys, split }: Completion,
	futures: &mut SendingFutures<'a>,
	statuses: &mut TransactionStatuses,
	wakes: &mut WakeQueue,
) {
	match result {
		| Ok(dest) => {
			let _cork = self.db.db.cork();

			self.db.delete_active_requests(&keys);
			self.handle_response_ok(dest, split, futures, statuses, wakes)
				.await;
		},
		| Err(error) =>
			self.handle_response_err(error, split, futures, statuses, wakes)
				.await,
	}
}

#[implement(Service)]
async fn handle_response_ok<'a>(
	&'a self,
	dest: Destination,
	split: Option<Split>,
	futures: &mut SendingFutures<'a>,
	statuses: &mut TransactionStatuses,
	wakes: &mut WakeQueue,
) {
	log_recovery(&dest, statuses);

	if let Some(split) = split
		&& let Some((items, split)) = self.split_delivered(&dest, split).await
	{
		run_status(&dest, statuses);
		futures.push(self.send_events(dest, items, Some(split)));
		return;
	}

	let next = match &dest {
		| Destination::Federation(server) =>
			self.federation_batch(&dest, server, NewEvents::new())
				.await,
		| _ => Selection::Events(self.resume_queued(&dest, &[]).await),
	};

	run_status(&dest, statuses);
	self.schedule_events(dest, next, futures, statuses, wakes);
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
	(dest, error): SendingError,
	split: Option<Split>,
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
		| dest @ Destination::Appservice(_) => {
			let forced = matches!(retry_action, RetryAction::Force).then(|| dest.clone());

			arm_appservice_wake(wakes, dest, tries);
			if let Some(dest) = forced {
				self.handle_force_retry(dest, futures, statuses, wakes)
					.await;
			}
		},
		| Destination::Federation(server) => {
			if tries > 0 {
				self.stalled
					.lock()
					.expect("locked")
					.insert(server.clone(), Some(Instant::now()));
			}

			let (split, tries) = self
				.split_failure(&server, &error, split, tries)
				.await;

			if let Some(split) = split {
				let status = TransactionStatus::Splitting { tries, split };

				statuses.insert(Destination::Federation(server.clone()), status);
			}

			self.arm_federation_wake(server, tries, wakes)
				.await;
		},
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
		| TransactionStatus::Splitting { tries, .. }
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
	wakes: &mut WakeQueue,
) {
	let Ok(selection) = self
		.select_events(&dest, NewEvents::new(), statuses)
		.await
	else {
		return;
	};

	self.schedule_events(dest, selection, futures, statuses, wakes);
}
