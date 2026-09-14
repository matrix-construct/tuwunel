use std::{
	cmp::Reverse,
	time::{Duration, SystemTime},
};

use ruma::OwnedServerName;
use tokio::time::Instant;
use tuwunel_core::{
	Error, error,
	error::error_chain,
	implement, trace,
	utils::{exponential_backoff_remaining_secs, rand::secs as rand_secs},
	warn,
};

use super::{SendingFutures, TransactionStatus, TransactionStatuses, WakeQueue};
use crate::{
	federation::ShouldAttempt,
	sending::{Destination, Msg, SendingEvent, Service},
};

const PUSH_FAILURE_STREAK: u32 = 4;
const WAKE_OVERFLOW_DELAY: Duration = Duration::from_hours(365 * 24);

#[implement(Service)]
pub(super) async fn drain_due_wakes<'a>(
	&'a self,
	futures: &mut SendingFutures<'a>,
	statuses: &mut TransactionStatuses,
	wakes: &mut WakeQueue,
) {
	let now = Instant::now();

	while wakes
		.peek()
		.is_some_and(|Reverse((due, _))| *due <= now)
	{
		let Reverse((_, dest)) = wakes.pop().expect("peeked entry");

		self.handle_wake(dest, futures, statuses, wakes)
			.await;
	}
}

#[implement(Service)]
#[tracing::instrument(name = "wake", level = "debug", skip_all)]
async fn handle_wake<'a>(
	&'a self,
	dest: Destination,
	futures: &mut SendingFutures<'a>,
	statuses: &mut TransactionStatuses,
	wakes: &mut WakeQueue,
) {
	let status = statuses.get(&dest);

	if matches!(status, Some(TransactionStatus::Running | TransactionStatus::RunningForceRetry)) {
		return;
	}

	if matches!(
		(&dest, status),
		(Destination::Push(..), Some(TransactionStatus::Retrying { .. }))
	) {
		trace!(?dest, "Dropping push wake while retry is in flight");
		return;
	}

	if let (Destination::Push(..), Some(remaining)) = (&dest, self.push_backoff_remaining(status))
	{
		if wakes
			.iter()
			.any(|Reverse((_, armed_dest))| armed_dest == &dest)
		{
			trace!(?dest, "Dropping stale push wake");
		} else {
			trace!(?dest, ?remaining, "Re-arming early push wake");
			arm_wake_in(wakes, dest, remaining);
		}

		return;
	}

	match dest {
		| Destination::Appservice(_) => {},
		| dest @ Destination::Push(..) =>
			self.handle_force_retry(dest, futures, statuses)
				.await,
		| Destination::Federation(server) =>
			self.handle_federation_wake(server, futures, statuses, wakes)
				.await,
	}
}

#[implement(Service)]
async fn handle_federation_wake<'a>(
	&'a self,
	server: OwnedServerName,
	futures: &mut SendingFutures<'a>,
	statuses: &mut TransactionStatuses,
	wakes: &mut WakeQueue,
) {
	let should_attempt = self
		.services
		.federation
		.should_attempt(&server)
		.await;

	let dest = Destination::Federation(server);

	match should_attempt {
		| ShouldAttempt::No { earliest_retry } => arm_wake(wakes, dest, earliest_retry),
		| _ => {
			let msg = Msg {
				dest,
				event: SendingEvent::Flush,
				queue_id: Vec::new(),
			};

			self.handle_request(msg, futures, statuses).await;
		},
	}
}

#[implement(Service)]
pub(super) async fn arm_federation_wake(&self, server: OwnedServerName, wakes: &mut WakeQueue) {
	if let ShouldAttempt::No { earliest_retry } = self
		.services
		.federation
		.should_attempt(&server)
		.await
	{
		arm_wake(wakes, Destination::Federation(server), earliest_retry);
	}
}

#[implement(Service)]
pub(super) fn arm_push_wake(
	&self,
	dest: Destination,
	error: &Error,
	statuses: &TransactionStatuses,
	wakes: &mut WakeQueue,
) {
	let Some(status @ TransactionStatus::Failed { tries, .. }) = statuses.get(&dest) else {
		return;
	};

	let delay = self
		.push_backoff_remaining(Some(status))
		.unwrap_or_default();

	let (deadline, retry_in) = wake_deadline(delay);

	record_push_failure(&dest, error, *tries, retry_in);
	wakes.push(Reverse((deadline, dest)));
}

#[implement(Service)]
#[inline]
pub(super) fn push_backoff_remaining(
	&self,
	status: Option<&TransactionStatus>,
) -> Option<Duration> {
	let Some(TransactionStatus::Failed { tries, last }) = status else {
		return None;
	};

	exponential_backoff_remaining_secs(
		self.server.config.sender_timeout,
		self.server.config.sender_retry_backoff_limit,
		last.elapsed(),
		*tries,
	)
}

fn record_push_failure(dest: &Destination, error: &Error, tries: u32, retry_in: Duration) {
	let Destination::Push(user_id, pushkey) = dest else {
		return;
	};

	match tries {
		| PUSH_FAILURE_STREAK => error!(
			%user_id,
			%pushkey,
			streak = tries,
			retry_in_seconds = retry_in.as_secs(),
			chain = %error_chain(error),
			"Push notifications for this pusher are not being delivered",
		),
		| _ => warn!(
			%user_id,
			%pushkey,
			streak = tries,
			retry_in_seconds = retry_in.as_secs(),
			chain = %error_chain(error),
			"Push transaction failed",
		),
	}
}

fn arm_wake(wakes: &mut WakeQueue, dest: Destination, earliest_retry: SystemTime) {
	let delay = earliest_retry
		.duration_since(SystemTime::now())
		.unwrap_or_default();

	arm_wake_in(wakes, dest, delay);
}

fn arm_wake_in(wakes: &mut WakeQueue, dest: Destination, delay: Duration) {
	let (deadline, _) = wake_deadline(delay);

	wakes.push(Reverse((deadline, dest)));
}

fn wake_deadline(delay: Duration) -> (Instant, Duration) {
	// Floor the delay at 1s so clock steps and past deadlines wake promptly.
	let delay = delay.max(Duration::from_secs(1));

	// Jitter by up to another delay-width (3s floor) so a backoff tier trickles back.
	let jitter = rand_secs(0..delay.as_secs().max(3));
	let now = Instant::now();
	let scheduled = delay.saturating_add(jitter);
	let deadline = now
		.checked_add(scheduled)
		.or_else(|| now.checked_add(WAKE_OVERFLOW_DELAY))
		.unwrap_or(now);

	let scheduled = deadline.saturating_duration_since(now);

	(deadline, scheduled)
}
