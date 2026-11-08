//! Email notifications for the `email` pusher kind.
//!
//! A notifying event for a user with an email pusher wakes the worker, which
//! waits out the configured delay and then mails every notification still
//! unread in one message. The pending work is derived from the stored
//! notification rows; only a cursor per pusher, and a throttle per room it has
//! mailed about, are kept beside them, so a restart loses no pending mail of
//! a pusher whose cursor has started.

mod evaluate;
mod render;
#[cfg(test)]
mod tests;

use std::{collections::BTreeMap, pin::pin, time::Duration};

use futures::{
	FutureExt, StreamExt,
	future::{Either, select},
	stream::unfold,
};
use loole::{Receiver, Sender, unbounded};
use ruma::{
	OwnedUserId, UserId,
	api::client::push::{PusherKind, set_pusher::v3::PusherAction},
};
use tokio::time::{Instant, sleep_until};
use tuwunel_core::{
	Result, implement,
	result::NotFound,
	smallstr::SmallString,
	utils::{
		MutexMap, MutexMapGuard, OptionExt, exponential_backoff_remaining, stream::TryIgnore,
		time::now_millis,
	},
	warn,
};
use tuwunel_database::{Deserialized, Interfix, Json};

pub use self::evaluate::Outcome;

/// One email pusher: its user, and its pushkey.
///
/// An email pusher's pushkey is the address it mails.
type PusherKey = (OwnedUserId, PushKey);

type PushKey = SmallString<[u8; 48]>;

/// Serializes the writes to each email pusher's rows.
///
/// It holds one entry per pusher with a holder: `put_pusher`, `delete_pusher`
/// and the worker's `start_cursor`, `record_state`, `forget_orphan` and
/// `forget_unbound`. None holds it across sending a mail.
pub(super) type Locks = MutexMap<PusherKey, ()>;

/// The email pushers with pending work and when each is next evaluated.
///
/// An entry lives from a pusher's first wake to the evaluation that mails it
/// or finds nothing left to mail, so only pushers with unread notifications
/// not yet mailed are held. A restart loses retry counts, harmlessly, since
/// every pusher with a cursor is evaluated again after startup. It also loses
/// any frontier kept for a cursor not yet started, so that pusher's cursor
/// starts at its next wake, past the notifications before it.
type Schedule = BTreeMap<PusherKey, Entry>;

/// The pusher one notification wakes, and the frontier of retired writes when
/// the wake was sent.
///
/// The frontier starts a cursor for a pusher that has none.
type Wake = (PusherKey, u64);

/// The worker's channel, carrying one wake per notification.
///
/// It is unbounded, so the append path never waits on the worker.
pub(super) struct Wakes {
	sender: Sender<Wake>,
	receiver: Receiver<Wake>,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
	due: Instant,
	failures: u32,

	/// The frontier to start the pusher's cursor at, kept while the cursor
	/// cannot be read.
	frontier: Option<u64>,
}

enum Next {
	Wake(Wake),
	Due(PusherKey),
}

/// The wait after startup before evaluating the pushers that had work.
///
/// It keeps the first evaluations out of the way of startup.
const BOOT_GRACE: Duration = Duration::from_mins(1);

const RETRY_MIN: Duration = Duration::from_mins(1);

const RETRY_MAX: Duration = Duration::from_hours(6);

/// The longest any pusher waits, whatever the configured delay.
///
/// It keeps a huge delay from overflowing the timer.
const WAIT_MAX: Duration = Duration::from_hours(30 * 24);

impl Default for Wakes {
	fn default() -> Self {
		let (sender, receiver) = unbounded();

		Self { sender, receiver }
	}
}

/// Stores a pusher under its email lock, starting a new email pusher's cursor
/// past every notification counted so far.
///
/// A stored pusher that is already an email pusher, or cannot be read, keeps
/// its cursor, so nothing it has yet to mail is skipped; a new one drops any
/// room throttles left by an earlier pusher with its pushkey. Taking the lock
/// for every kind orders the store against the worker deleting the pusher it
/// replaces.
#[implement(super::Service)]
pub(super) async fn put_pusher(&self, user_id: &UserId, pushkey: &str, action: &PusherAction) {
	let email = matches!(
		action,
		PusherAction::Post(data) if matches!(data.pusher.kind, PusherKind::Email(_))
	);

	let _lock = self.email_lock(user_id, pushkey).await;
	let created = email && matches!(self.email_pusher(user_id, pushkey).await, Ok(None));

	if created {
		let cursor = self.services.globals.pending_count().end;

		self.forget_throttles(user_id, pushkey).await;
		self.put_cursor(user_id, pushkey, cursor);
	}

	self.db
		.senderkey_pusher
		.put((user_id, pushkey), Json(action));
}

#[implement(super::Service)]
pub(super) async fn email_lock(
	&self,
	user_id: &UserId,
	pushkey: &str,
) -> MutexMapGuard<PusherKey, ()> {
	self.email_locks
		.lock(&pusher_key(user_id, pushkey))
		.await
}

fn pusher_key(user_id: &UserId, pushkey: &str) -> PusherKey {
	(user_id.to_owned(), pushkey.into())
}

/// Deletes a pusher's cursor and room throttles.
///
/// Callers hold the pusher's lock.
#[implement(super::Service)]
pub(super) async fn forget_email_state(&self, user_id: &UserId, pushkey: &str) {
	self.db
		.senderkey_emailcursor
		.del((user_id, pushkey));

	self.forget_throttles(user_id, pushkey).await;
}

/// Deletes a pusher's room throttles.
///
/// Callers hold the pusher's lock.
#[implement(super::Service)]
async fn forget_throttles(&self, user_id: &UserId, pushkey: &str) {
	self.db
		.senderkeyroom_emailthrottle
		.del_prefix(&(user_id, pushkey, Interfix))
		.await;
}

/// Writes a pusher's cursor, the count of the last notification row it
/// processed.
///
/// Callers hold the pusher's lock.
#[implement(super::Service)]
fn put_cursor(&self, user_id: &UserId, pushkey: &str, cursor: u64) {
	self.db
		.senderkey_emailcursor
		.put((user_id, pushkey), cursor);
}

/// Wakes the worker for one notification an email pusher may mail.
///
/// Nothing is sent while mail is off, so no pusher starts a cursor that would
/// later mail everything since; a wake sent after the worker has stopped is
/// dropped.
#[implement(super::Service)]
pub(super) fn wake_email(&self, user_id: &UserId, pushkey: &str) {
	if !self.services.sendmail.is_enabled() {
		return;
	}

	let frontier = self.services.globals.current_count();
	let wake = (pusher_key(user_id, pushkey), frontier);

	self.email_wakes.sender.send(wake).ok();
}

/// Closes the worker's channel, which ends the worker.
///
/// The worker first handles the wakes already sent.
#[implement(super::Service)]
pub(super) fn close_email_wakes(&self) { self.email_wakes.sender.close(); }

/// Service worker: schedules woken email pushers and evaluates each when due.
///
/// Evaluations run one at a time. Once the channel closes at shutdown, the
/// worker finishes the evaluation in flight and starts no other. It still
/// handles every wake already sent, so each woken pusher keeps its cursor.
#[implement(super::Service)]
#[tracing::instrument(name = "email", level = "debug", skip_all)]
pub(super) async fn email_worker(&self) {
	let schedule = self.boot_schedule().await;

	unfold(schedule, async |schedule| {
		let schedule = match self.wait_next(&schedule).await? {
			| Next::Wake(wake) => self.schedule_wake(schedule, wake).await,
			| Next::Due(key) => self.evaluate_due(schedule, key).await,
		};

		Some(((), schedule))
	})
	.collect::<()>()
	.await;
}

/// Schedules every email pusher with a cursor.
///
/// Mail pending across a restart then goes out without waiting for a fresh
/// notification; a pusher with nothing left evaluates to idle.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn boot_schedule(&self) -> Schedule {
	let entry = entry_in(BOOT_GRACE, 0);

	self.db
		.senderkey_emailcursor
		.keys()
		.ignore_err()
		.map(|(user_id, pushkey): (&UserId, &str)| (pusher_key(user_id, pushkey), entry))
		.collect()
		.await
}

/// Waits for the next wake, or for the earliest scheduled pusher to fall due.
///
/// A pusher already due is returned before any waiting wake, so a steady run
/// of wakes cannot hold it back. Once the channel is closed, only the wakes
/// already sent are returned, then `None`.
#[implement(super::Service)]
async fn wait_next(&self, schedule: &Schedule) -> Option<Next> {
	let receiver = &self.email_wakes.receiver;
	let received = receiver
		.recv_async()
		.map(|wake| wake.ok().map(Next::Wake));

	let Some((key, entry)) = schedule
		.iter()
		.min_by_key(|(_, entry)| entry.due)
		.filter(|_| !receiver.is_closed())
	else {
		return received.await;
	};

	match select(pin!(sleep_until(entry.due)), pin!(received)).await {
		| Either::Left(((), _)) if !receiver.is_closed() => Some(Next::Due(key.clone())),
		| Either::Left(((), received)) => received.await,
		| Either::Right((wake, _)) => wake,
	}
}

/// Schedules a woken pusher one delay out, unless it is already due sooner or
/// backing off.
///
/// It first starts the pusher's cursor when it has none, at the earliest
/// frontier kept by a wake that could not read the cursor, else at this wake's.
#[implement(super::Service)]
async fn schedule_wake(&self, schedule: Schedule, (key, frontier): Wake) -> Schedule {
	let frontier = kept_frontier(&schedule, &key).map_or(frontier, |kept| kept.min(frontier));
	let kept = self.start_cursor(&key, frontier).await;
	let delay = Duration::from_secs(self.services.config.push.email_delay);

	woken(schedule, key, Entry { frontier: kept, ..entry_in(delay, 0) })
}

/// The frontier kept for a pusher whose cursor could not be started, if any.
fn kept_frontier(schedule: &Schedule, key: &PusherKey) -> Option<u64> {
	schedule.get(key)?.frontier
}

/// Starts the cursor of a woken pusher that has none, at `frontier`, returning
/// the frontier when its read fails but may succeed on a retry.
///
/// A pusher stored before email notifications existed has none, so nothing
/// written before its first wake is mailed, while a notification still being
/// written when the wake was sent is. A pusher deleted since the wake gets one
/// too, which its evaluation deletes. A returned frontier is kept to start the
/// cursor again, since evaluating without one would skip the notification,
/// while a cursor unreadable for good is left alone.
#[implement(super::Service)]
async fn start_cursor(&self, key: &PusherKey, frontier: u64) -> Option<u64> {
	let _lock = self.email_locks.lock(key).await;
	let (user_id, pushkey) = key;

	match self.email_cursor(user_id, pushkey).await {
		| Ok(Some(_)) => None,
		| Err(error) => error.is_transient_io().then_some(frontier),
		| Ok(None) => {
			self.put_cursor(user_id, pushkey, frontier);

			None
		},
	}
}

/// The pusher's cursor, or `None` when it has none.
///
/// A cursor that cannot be read is logged and is an error, never taken for its
/// absence.
#[implement(super::Service)]
async fn email_cursor(&self, user_id: &UserId, pushkey: &str) -> Result<Option<u64>> {
	self.db
		.senderkey_emailcursor
		.qry(&(user_id, pushkey))
		.await
		.deserialized()
		.present()
		.inspect_err(|error| warn!(%user_id, %error, "Failed to read an email cursor"))
}

/// Evaluates a pusher that has fallen due.
///
/// A frontier kept by an earlier wake starts the cursor first. While that still
/// fails, the pusher backs off unevaluated, keeping the frontier.
#[implement(super::Service)]
async fn evaluate_due(&self, schedule: Schedule, key: PusherKey) -> Schedule {
	let kept = kept_frontier(&schedule, &key)
		.map_async(|frontier| self.start_cursor(&key, frontier))
		.await
		.flatten();

	if kept.is_some() {
		return reschedule(schedule, key, Outcome::Failed, kept);
	}

	let (user_id, pushkey) = &key;
	let outcome = self
		.evaluate_email(user_id, pushkey, now_millis())
		.await;

	reschedule(schedule, key, outcome, None)
}

/// Adds a woken pusher, or brings its entry forward to the sooner due time.
///
/// An entry counting failures keeps its backoff. The wake's frontier replaces
/// the entry's; it already folds in any kept one.
fn woken(mut schedule: Schedule, key: PusherKey, entry: Entry) -> Schedule {
	schedule
		.entry(key)
		.and_modify(|current| {
			current.frontier = entry.frontier;
			if current.failures == 0 {
				current.due = current.due.min(entry.due);
			}
		})
		.or_insert(entry);

	schedule
}

/// Applies an outcome to a pusher that fell due.
///
/// The outcome, an evaluation's or a failed cursor start's, decides the
/// pusher's next entry, if any, carrying its failure count forward; the entry
/// keeps the frontier of a cursor start that failed.
fn reschedule(
	mut schedule: Schedule,
	key: PusherKey,
	outcome: Outcome,
	kept: Option<u64>,
) -> Schedule {
	let failures = schedule
		.remove(&key)
		.map_or(0, |entry| entry.failures);

	let entry = next_entry(outcome, failures).map(|entry| Entry { frontier: kept, ..entry });

	schedule.extend(entry.map(|entry| (key, entry)));
	schedule
}

/// The entry a pusher gets after an outcome, if any.
///
/// A sent or idle pusher leaves the schedule, a pending one returns at its due
/// time, and a failed one backs off from `RETRY_MIN` up to `RETRY_MAX`.
fn next_entry(outcome: Outcome, failures: u32) -> Option<Entry> {
	match outcome {
		| Outcome::Sent | Outcome::Idle => None,
		| Outcome::Pending(due) => {
			let wait = Duration::from_millis(due.saturating_sub(now_millis()));

			Some(entry_in(wait, 0))
		},
		| Outcome::Failed => {
			let failures = failures.saturating_add(1);
			let wait =
				exponential_backoff_remaining(RETRY_MIN, RETRY_MAX, Duration::ZERO, failures)
					.unwrap_or(RETRY_MIN);

			Some(entry_in(wait, failures))
		},
	}
}

fn entry_in(wait: Duration, failures: u32) -> Entry {
	let now = Instant::now();
	let due = now.checked_add(wait.min(WAIT_MAX)).unwrap_or(now);

	Entry { due, failures, frontier: None }
}
