use std::{
	cmp::Reverse,
	collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet, btree_map::Entry},
	fmt::Debug,
	iter::once,
	str::from_utf8,
	sync::{
		Arc,
		atomic::{AtomicU64, AtomicUsize, Ordering},
	},
	time::{Duration, Instant, SystemTime},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures::{
	FutureExt, StreamExt, TryFutureExt,
	future::{BoxFuture, join, join3, try_join3},
	pin_mut,
	stream::FuturesUnordered,
};
use ruma::{
	MilliSecondsSinceUnixEpoch, OneTimeKeyAlgorithm, OwnedDeviceId, OwnedRoomId, OwnedServerName,
	OwnedUserId, RoomId, ServerName, UInt, UserId,
	api::{
		appservice::event::push_events::v1::{
			DeviceLists, EphemeralData, Request as PushEventsRequest,
		},
		client::push::Pusher,
		error::ErrorKind,
		federation::transactions::edu::{
			DeviceListUpdateContent, Edu, PresenceContent, PresenceUpdate, ReceiptContent,
			ReceiptData, ReceiptMap,
		},
	},
	device_id,
	events::{
		AnySyncEphemeralRoomEvent, GlobalAccountDataEventType, push_rules::PushRulesEvent,
		receipt::ReceiptType,
	},
	presence::PresenceState,
	push::Ruleset,
	serde::Raw,
	uint,
};
use serde::Deserialize;
use tokio::{
	select,
	time::{Instant as TokioInstant, sleep_until},
};
use tuwunel_core::{
	Error, Event, Result, debug, debug_warn, err, error,
	error::error_chain,
	extract_variant, implement,
	result::LogErr,
	smallvec::SmallVec,
	trace,
	utils::{
		BoolExt, ReadyExt, calculate_hash, exponential_backoff_remaining_secs,
		future::TryExtExt,
		rand::secs as rand_secs,
		stream::{BroadbandExt, IterStream, WidebandExt},
	},
	warn,
};

use super::{
	Destination, EduBuf, EduVec, Msg, SendingEvent, Service, TAG_PREFIX_LEN, data::QueueItem,
	reap_flushes,
};
use crate::{federation::ShouldAttempt, rooms::timeline::RawPduId};

mod dispatch_appservice;
mod dispatch_federation;
mod dispatch_push;
mod response;
mod select;
#[cfg(test)]
mod tests;

/// Output of one EDU selector. `shipped` rides the current transaction up to
/// the shared budget; `overflow` past the budget is written as queued rows for
/// later transactions to drain.
#[derive(Default)]
struct Selected {
	shipped: EduVec,
	overflow: Vec<EduBuf>,
}

/// The appservice-injected recipient fields of a queued to-device event
/// (MSC4203), parsed to scope MSC3202 one-time-key counts to the addressed
/// devices.
#[derive(Deserialize)]
struct ToDeviceRecipient {
	to_user_id: OwnedUserId,
	to_device_id: OwnedDeviceId,
}

#[derive(Default)]
struct PushFailures {
	ids: FailedPushIds,
	error: Option<Error>,
}

/// In-flight bookkeeping for one `Destination`. Cross-attempt backoff lives
/// in `peer_status` (federation only); appservice/push paths keep their own
/// status because they are not server-keyed.
#[derive(Debug)]
enum TransactionStatus {
	// A durable active generation awaiting its first dispatch after restart.
	Pending,
	Running,
	RunningForceRetry,
	Failed(u32, Instant), // push backoff: tries, last failure
	Retrying(u32),        // number of times failed
}

enum RetryAction {
	None,
	Force,
}

type SendingError = (Destination, Error);
type SendingResult = Result<Destination, SendingError>;
type SendingFuture<'a> = BoxFuture<'a, SendingResult>;
type SendingFutures<'a> = FuturesUnordered<SendingFuture<'a>>;
type CurTransactionStatus = HashMap<Destination, TransactionStatus>;
type FailedPushIds = SmallVec<[RawPduId; 1]>;

/// MSC3202 `device_one_time_keys_count`: unclaimed one-time-key counts per
/// algorithm, keyed by user then device. Matches the ruma request field type.
type OtkCounts =
	BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceId, BTreeMap<OneTimeKeyAlgorithm, UInt>>>;

/// MSC3202 `device_unused_fallback_key_types`: algorithms with an unused
/// fallback key, keyed by user then device.
type FallbackTypes = BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceId, Vec<OneTimeKeyAlgorithm>>>;

/// The MSC3202-interesting devices of one transaction: the appservice
/// sender's plus matched PDU senders' devices and the to-device recipients.
type Devices = SmallVec<[(OwnedUserId, OwnedDeviceId); 1]>;

/// Per-worker retry timer keyed by earliest-retry deadline and destination.
///
/// Every recorded federation or push failure arms an entry. Stale entries are
/// consumed by the destination's in-flight or newer failure generation. The
/// heap is bounded by concurrently failing destinations, transient federation
/// re-arms, and stale push entries.
type WakeQueue = BinaryHeap<Reverse<(TokioInstant, Destination)>>;

/// Per-(room, user) bucket of `ReceiptData`. MSC3771 allows one receipt
/// per thread context per user per EDU window; the dominant case is
/// still a single receipt, so inline-1 fits without a heap touch.
type UserReceipts = SmallVec<[ReceiptData; 1]>;

/// Per-rank slice of receipt EDU output. Each entry becomes one
/// `Edu::Receipt` buffer; rank 0 carries each user's earliest receipt
/// in the window, rank 1 the next, and so on. Most windows produce a
/// single rank.
type RankedReceipts = SmallVec<[ReceiptMap; 1]>;

/// Per-room ranked receipts gathered for one federation EDU window. The
/// common case is a single room, so inline-1 avoids a heap touch.
type RoomReceipts = SmallVec<[(OwnedRoomId, RankedReceipts); 1]>;

impl PushFailures {
	fn retain(mut self, pdu_id: RawPduId, error: Error) -> Self {
		self.ids.push(pdu_id);
		self.error = self.error.or(Some(error));

		self
	}
}

const SELECT_PRESENCE_LIMIT: usize = 256;
const SELECT_RECEIPT_LIMIT: usize = 256;
const DEQUEUE_LIMIT: usize = 48;
const PUSH_FAILURE_STREAK: u32 = 4;
const WAKE_OVERFLOW_DELAY_SECS: u64 = 365 * 24 * 60 * 60;
const WAKE_OVERFLOW_DELAY: Duration = Duration::from_secs(WAKE_OVERFLOW_DELAY_SECS);

pub const PDU_LIMIT: usize = 50;
pub const EDU_LIMIT: usize = 100;

impl Service {
	#[tracing::instrument(skip(self), level = "debug")]
	pub(super) async fn sender(self: Arc<Self>, id: usize) -> Result {
		let mut statuses: CurTransactionStatus = CurTransactionStatus::new();
		let mut futures: SendingFutures<'_> = FuturesUnordered::new();
		let mut wakes: WakeQueue = WakeQueue::new();

		self.startup_netburst(id, &mut futures, &mut statuses)
			.boxed()
			.await;

		self.work_loop(id, &mut futures, &mut statuses, &mut wakes)
			.await;

		if !futures.is_empty() {
			self.finish_responses(&mut futures).boxed().await;
		}

		Ok(())
	}

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
		statuses: &mut CurTransactionStatus,
		wakes: &mut WakeQueue,
	) {
		let receiver = self
			.channels
			.get(id)
			.map(|(_, receiver)| receiver.clone())
			.expect("Missing channel for sender worker");

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

	#[tracing::instrument(name = "response", level = "debug", skip_all)]
	async fn handle_response<'a>(
		&'a self,
		response: SendingResult,
		futures: &mut SendingFutures<'a>,
		statuses: &mut CurTransactionStatus,
		wakes: &mut WakeQueue,
	) {
		match response {
			| Ok(dest) =>
				self.handle_response_ok(&dest, futures, statuses)
					.await,
			| Err((dest, e)) => {
				let retry_action = Self::handle_response_err(&dest, statuses, &e);

				match dest {
					| Destination::Federation(server) => {
						// Arm a one-shot retry at the destination's earliest-retry time.
						if let ShouldAttempt::No { earliest_retry } = self
							.services
							.federation
							.should_attempt(&server)
							.await
						{
							arm_wake(wakes, Destination::Federation(server), earliest_retry);
						}
					},
					| dest @ Destination::Push(..) => {
						let Some(status @ TransactionStatus::Failed(tries, _)) =
							statuses.get(&dest)
						else {
							return;
						};

						let tries = *tries;
						let delay = self
							.push_backoff_remaining(Some(status))
							.unwrap_or_default();

						let (deadline, retry_in) = wake_deadline(delay);

						Self::record_push_failure(&dest, &e, tries, retry_in);
						wakes.push(Reverse((deadline, dest)));
					},
					| dest if matches!(retry_action, RetryAction::Force) =>
						self.handle_force_retry(dest, futures, statuses)
							.await,
					| _ => {},
				}
			},
		}
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

fn wake_deadline(delay: Duration) -> (TokioInstant, Duration) {
	// Floor the delay at 1s so clock steps and past deadlines wake promptly.
	let delay = delay.max(Duration::from_secs(1));

	// Spread the wake over another delay-width (3s minimum), so destinations
	// sharing a backoff tier trickle back rather than retrying in one burst.
	let jitter = rand_secs(0..delay.as_secs().max(3));
	let now = TokioInstant::now();
	let scheduled = delay.saturating_add(jitter);
	let deadline = now.checked_add(scheduled).unwrap_or_else(|| {
		now.checked_add(WAKE_OVERFLOW_DELAY)
			.unwrap_or(now)
	});

	let scheduled = deadline.saturating_duration_since(now);

	(deadline, scheduled)
}

#[implement(Service)]
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

#[implement(Service)]
#[inline]
fn push_backoff_remaining(&self, status: Option<&TransactionStatus>) -> Option<Duration> {
	let Some(TransactionStatus::Failed(tries, time)) = status else {
		return None;
	};

	exponential_backoff_remaining_secs(
		self.server.config.sender_timeout,
		self.server.config.sender_retry_backoff_limit,
		time.elapsed(),
		*tries,
	)
}
