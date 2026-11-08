//! Delayed events (MSC4140): events scheduled now and sent later.
//!
//! A scheduled event is three rows written in one batch: its record, a
//! due-time index entry, and an owner index entry. The worker sends each event
//! when it falls due through the ordinary append path and then deletes its
//! rows whatever the outcome, so events overdue at startup go out on its first
//! pass and a crash between the two steps can send one twice.

use std::{pin::pin, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures::{
	Stream, StreamExt,
	future::{Either, select},
	stream::unfold,
};
use http::StatusCode;
use ruma::{
	MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedUserId, UserId,
	api::error::{ErrorKind, LimitExceededErrorData, RetryAfter},
	events::TimelineEventType,
};
use serde::{Deserialize, Serialize};
use tokio::{sync::Notify, time::timeout};
use tuwunel_core::{
	Err, Error, Result, debug_warn, err, error, implement,
	result::NotFound,
	smallstr::SmallString,
	utils::{
		MutexMap,
		rand::string_array,
		stream::{BroadbandExt, ReadyExt, TryIgnore, WidebandExt, automatic_width},
		time::now_millis,
	},
	warn,
};
use tuwunel_database::{Deserialized, Ignore, Interfix, Json, Map};
use tuwunel_matrix::{
	StateKey,
	pdu::{Content, PduBuilder},
};

use crate::{
	service::make_name,
	transaction_ids::{RoomScope, RoomTxn},
};

/// A `delay_id`, as this server issues it or a client names it.
///
/// One this server issued fits inline; a longer one, which only a client can
/// send, spills to the heap.
pub type DelayId = SmallString<[u8; DELAY_ID_LEN]>;

/// Schedules delayed events and sends them when they fall due.
///
/// Every operation on one event holds that event's lock, then the room's lock
/// when it sends, never the reverse.
pub struct Service {
	services: Arc<crate::services::OnceServices>,
	db: Data,

	/// Serializes the operations on one `delay_id`.
	///
	/// An entry lives only while held, so its size is bounded by concurrent
	/// operations.
	locks: MutexMap<DelayId, ()>,

	/// Wakes the worker when a new event may fall due before its next pass.
	wake: Notify,
}

struct Data {
	delayid_event: Arc<Map>,
	dueat_delayid: Arc<Map>,
	userid_delayid: Arc<Map>,
}

/// A scheduled event as stored, and as reported to its owner.
///
/// Fields are additive: a record missing a later field reads with its default.
#[derive(Debug, Deserialize, Serialize)]
pub struct Event {
	/// The user who scheduled the event and is its sender.
	pub user_id: OwnedUserId,

	/// The room the event is sent to.
	pub room_id: OwnedRoomId,

	/// The event's type.
	#[serde(rename = "type")]
	pub event_type: TimelineEventType,

	/// The state key of a state event, absent for a message event.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub state_key: Option<StateKey>,

	/// The content as the client sent it.
	pub content: Content,

	/// The target of a redaction, lifted into the top-level `redacts` field.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub redacts: Option<OwnedEventId>,

	/// The `origin_server_ts` an appservice asked for.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub timestamp: Option<MilliSecondsSinceUnixEpoch>,

	/// The delay requested at scheduling, which a restart counts again.
	pub delay_ms: u64,

	/// When the event falls due, as Unix time in milliseconds.
	pub due_ms: u64,
}

/// The limits a client is told about and held to.
///
/// Both come from the server configuration, and [`Service::limits`] reports
/// none while either is zero.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
	/// The longest delay a client may request, in milliseconds.
	pub max_delay_ms: u64,

	/// How many events one user may have scheduled at once.
	pub max_scheduled: usize,
}

// 32 alphanumerics carry about 190 random bits.
const DELAY_ID_LEN: usize = 32;

// Sent at once, so one wedged room cannot hold up the rest.
const SEND_WIDTH: usize = 8;

const MAX_WAIT: Duration = Duration::from_mins(1);

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		let db = Data {
			delayid_event: args.db["delayid_event"].clone(),
			dueat_delayid: args.db["dueat_delayid"].clone(),
			userid_delayid: args.db["userid_delayid"].clone(),
		};

		Ok(Arc::new(Self {
			services: args.services.clone(),
			db,
			locks: MutexMap::new(),
			wake: Notify::new(),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		self.due_events()
			.for_each_concurrent(SEND_WIDTH, async |(due, delay_id)| {
				self.send_scheduled(&delay_id, due).await;
			})
			.await;

		Ok(())
	}

	fn name(&self) -> &str { make_name(module_path!()) }
}

/// Returns the limits in force, or `None` while delayed events are disabled.
///
/// Either configured limit at zero disables the feature.
#[implement(Service)]
#[must_use]
pub fn limits(&self) -> Option<Limits> {
	let config = &self.services.config.rooms.delayed_events;
	let max_delay_ms = config.max_delay_s.saturating_mul(1000);
	let enabled = max_delay_ms > 0 && config.max_scheduled > 0;

	enabled.then_some(Limits {
		max_delay_ms,
		max_scheduled: config.max_scheduled,
	})
}

/// Refuses a user who already has `max_scheduled` events scheduled.
///
/// The count is read before the write without a lock, so concurrent requests
/// can overshoot it slightly. At the cap, entries left by dropped records are
/// reclaimed before refusing. The refusal's `Retry-After` is the time until the
/// user's next event falls due, rounded up to a second.
#[implement(Service)]
pub async fn check_capacity(&self, user_id: &UserId, max_scheduled: usize) -> Result {
	let scheduled = self
		.db
		.userid_delayid
		.count_prefix(&(user_id, Interfix))
		.await;

	if scheduled < max_scheduled || self.reclaim_orphans(user_id).await < max_scheduled {
		return Ok(());
	}

	let retry_after = self
		.next_due_of(user_id)
		.await
		.map(|due| due.saturating_sub(now_millis()).div_ceil(1000))
		.map(Duration::from_secs);

	Err(limit_exceeded(retry_after))
}

/// Returns the `delay_id` an earlier scheduling with this transaction stored.
///
/// The stored response is the `delay_id` itself, so a retried request answers
/// with the event it already scheduled.
#[implement(Service)]
pub async fn existing_txn(&self, txn: RoomTxn<'_>, event_type: &str) -> Result<Option<DelayId>> {
	self.services
		.transaction_ids
		.existing_room_txnid(txn, RoomScope::Delayed(event_type))
		.await
		.optional()?
		.map(|response| {
			str::from_utf8(&response)
				.map(DelayId::from)
				.map_err(|e| err!(Database("Unreadable delayed event transaction response: {e}")))
		})
		.transpose()
}

/// Stores a new scheduled event and returns its `delay_id`.
///
/// The client's transaction response, when given, commits in the same batch
/// as the event's rows. The worker is woken in case the event falls due
/// before its next pass.
#[implement(Service)]
pub fn schedule(&self, event: &Event, txn: Option<RoomTxn<'_>>) -> DelayId {
	let delay_id = DelayId::from(string_array::<DELAY_ID_LEN>().as_str());
	let event_type = event.event_type.to_cow_str();
	let response = txn.map(|txn| (txn, RoomScope::Delayed(&event_type)));

	self.insert(&delay_id, event, response);
	self.wake.notify_one();

	delay_id
}

/// Reads a scheduled event, as missing unless `owner` scheduled it.
///
/// A tokenless management request passes no owner; anything that names one
/// is held to it.
#[implement(Service)]
pub async fn get(&self, delay_id: &str, owner: Option<&UserId>) -> Result<Event> {
	self.get_event(delay_id)
		.await
		.ok()
		.filter(|event| owner.is_none_or(|owner| owner == event.user_id))
		.ok_or_else(|| err!(Request(NotFound("No scheduled delayed event with this ID."))))
}

/// Streams the events a user has scheduled, with their `delay_id`s.
///
/// An index entry whose record is missing or unreadable is skipped.
#[implement(Service)]
pub fn scheduled<'a>(
	&'a self,
	user_id: &'a UserId,
) -> impl Stream<Item = (DelayId, Event)> + Send + 'a {
	self.owned_ids(user_id)
		.wide_filter_map(async |delay_id| {
			let event = self.get_event(&delay_id).await.ok()?;

			Some((delay_id, event))
		})
}

/// Counts a scheduled event's delay again from now.
///
/// The event keeps its `delay_id` and content; only its due time and its
/// due-time index entry move.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn restart(&self, delay_id: &str, owner: Option<&UserId>) -> Result {
	let _lock = self.locks.lock(&DelayId::from(delay_id)).await;
	let event = self.get(delay_id, owner).await?;
	let old_due = event.due_ms;
	let event = Event {
		due_ms: now_millis().saturating_add(event.delay_ms),
		..event
	};

	self.reschedule(delay_id, &event, old_due);

	Ok(())
}

/// Cancels a scheduled event.
///
/// Its rows are deleted in one batch, so it can no longer be read or sent.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn cancel(&self, delay_id: &str, owner: Option<&UserId>) -> Result {
	let _lock = self.locks.lock(&DelayId::from(delay_id)).await;
	let event = self.get(delay_id, owner).await?;

	self.remove(delay_id, &event.user_id, event.due_ms);

	Ok(())
}

/// Sends a scheduled event now and removes it whatever the outcome.
///
/// An append can fail after the event is stored, so a failed send is never
/// kept for another attempt. The error is the one an ordinary send returns.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn send(&self, delay_id: &str, owner: Option<&UserId>) -> Result<OwnedEventId> {
	let _lock = self.locks.lock(&DelayId::from(delay_id)).await;
	let event = self.get(delay_id, owner).await?;

	self.send_and_remove(delay_id, event).await
}

/// Removes every event a user has scheduled.
///
/// Called on deactivation, after the account's devices are removed; an index
/// entry whose record is already gone is dropped as well. A send already
/// holding its event's lock completes, and a request authenticated before the
/// devices were removed can still schedule an event after this returns.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn remove_user(&self, user_id: &UserId) {
	self.owned_ids(user_id)
		.for_each_concurrent(automatic_width(), async |delay_id| {
			self.remove_owned(user_id, &delay_id).await;
		})
		.await;
}

/// Streams events as they fall due, until shutdown.
///
/// Each pass reads the due-time index up to now, passing over events whose
/// send is still in flight, then waits for the next deadline, a newly
/// scheduled event, or shutdown. A send that hangs holds one of the worker's
/// slots, never the passes after it.
#[implement(Service)]
fn due_events(&self) -> impl Stream<Item = (u64, DelayId)> + Send + '_ {
	unfold(None, async move |last: Option<u64>| {
		if let Some(last) = last {
			self.wait_for_due(last).await?;
		}

		let now = now_millis();

		Some((self.due_at(now), Some(now)))
	})
	.flatten()
}

/// Waits for the first deadline after `last`, a newly scheduled event, or
/// shutdown, returning `None` on shutdown.
///
/// The wait is capped, so a lost wake costs at most one more minute.
#[implement(Service)]
async fn wait_for_due(&self, last: u64) -> Option<()> {
	let wait = self
		.next_due_after(last)
		.await
		.map_or(MAX_WAIT, |due| Duration::from_millis(due.saturating_sub(now_millis())))
		.min(MAX_WAIT);

	let wake = timeout(wait, self.wake.notified());
	let shutdown = self.services.server.until_shutdown();

	match select(pin!(wake), pin!(shutdown)).await {
		| Either::Left(_) => Some(()),
		| Either::Right(_) => None,
	}
}

/// Streams the events due at `now` whose send is not already in flight.
///
/// One unreadable index entry is logged and passed over; nothing a single
/// row holds can end the worker.
#[implement(Service)]
fn due_at(&self, now: u64) -> impl Stream<Item = (u64, DelayId)> + Send + '_ {
	self.db
		.dueat_delayid
		.keys()
		.ready_take_while(move |key: &Result<(u64, &str)>| {
			key.as_ref().is_ok_and(|&(due, _)| due <= now) || key.is_err()
		})
		.ready_filter_map(|key| {
			key.inspect_err(|e| error!(%e, "Unreadable delayed event index entry"))
				.ok()
		})
		.map(|(due, delay_id)| (due, DelayId::from(delay_id)))
		.ready_filter(|(_, delay_id)| !self.locks.contains(delay_id))
}

/// Sends one due event, unless it moved or went since the index was read.
///
/// A due entry whose record is gone is deleted, and an unreadable record is
/// dropped with its due entry, so no row can stall the worker. A refusal by
/// the room's rules has no remedy, so only a server fault logs in a release
/// build.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self))]
async fn send_scheduled(&self, delay_id: &DelayId, due: u64) {
	let _lock = self.locks.lock(delay_id).await;
	let event = match self.get_event(delay_id).await {
		| Ok(event) if event.due_ms == due => event,
		| Ok(_) => return,
		| Err(e) if e.is_not_found() => return self.drop_due(delay_id, due),
		| Err(e) => {
			error!(%delay_id, %e, "Dropping unreadable delayed event");
			return self.drop_due(delay_id, due);
		},
	};

	let user_id = event.user_id.clone();

	self.send_and_remove(delay_id, event)
		.await
		.inspect_err(|e| match e.status_code().is_server_error() {
			| true => warn!(%delay_id, %user_id, %e, "Delayed event not sent"),
			| false => debug_warn!(%delay_id, %user_id, %e, "Delayed event not sent"),
		})
		.ok();
}

/// Sends an event, then deletes its rows whatever the outcome.
///
/// Sending first means a crash between the two steps repeats the event rather
/// than losing it.
#[implement(Service)]
async fn send_and_remove(&self, delay_id: &str, event: Event) -> Result<OwnedEventId> {
	let user_id = event.user_id.clone();
	let due = event.due_ms;
	let result = self.append(event).await;

	self.remove(delay_id, &user_id, due);

	result
}

/// Appends an event through the ordinary local send path.
///
/// A suspended sender is refused here, since the suspension may postdate the
/// scheduling.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn append(&self, event: Event) -> Result<OwnedEventId> {
	let Event {
		user_id,
		room_id,
		event_type,
		state_key,
		content,
		redacts,
		timestamp,
		..
	} = event;

	if self.services.users.is_suspended(&user_id).await {
		return Err!(Request(UserSuspended("Cannot send delayed events while suspended.")));
	}

	let pdu = PduBuilder {
		event_type,
		content,
		unsigned: None,
		state_key,
		redacts,
		timestamp,
	};

	let state_lock = self.services.state.mutex.lock(&room_id).await;

	self.services
		.timeline
		.build_and_append_pdu(pdu, &user_id, &room_id, &state_lock)
		.await
}

/// Removes one of a user's events during deactivation.
///
/// An owner index entry whose record is gone or unreadable is deleted with
/// that record; the worker drops its due entry when it falls due.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self))]
async fn remove_owned(&self, user_id: &UserId, delay_id: &DelayId) {
	let _lock = self.locks.lock(delay_id).await;

	match self.get_event(delay_id).await {
		| Ok(event) => self.remove(delay_id, &event.user_id, event.due_ms),
		| Err(_) => self.drop_owned(user_id, delay_id),
	}
}

/// Deletes a user's owner index entries whose record is gone or unreadable,
/// and counts the events left.
///
/// It runs only at the cap, so an entry the worker could not clean up after a
/// dropped record cannot hold one of the user's slots for good.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self))]
async fn reclaim_orphans(&self, user_id: &UserId) -> usize {
	self.owned_ids(user_id)
		.broad_filter_map(async |delay_id| {
			let _lock = self.locks.lock(&delay_id).await;

			self.get_event(&delay_id)
				.await
				.inspect_err(|_| self.drop_owned(user_id, &delay_id))
				.ok()
		})
		.count()
		.await
}

/// Returns when the first event due after `now` falls due.
///
/// The worker sleeps until then, so an unreadable entry is logged and read as
/// no event at all.
#[implement(Service)]
#[tracing::instrument(level = "trace", skip(self))]
async fn next_due_after(&self, now: u64) -> Option<u64> {
	pin!(
		self.db
			.dueat_delayid
			.keys_from(&(now.saturating_add(1),))
	)
	.next()
	.await
	.and_then(|key| {
		key.inspect_err(|e| error!(%e, "Unreadable delayed event index entry"))
			.ok()
	})
	.map(|(due, _): (u64, Ignore)| due)
}

/// Returns when the user's next event falls due.
///
/// It sets the `Retry-After` of a refusal at capacity, so it reads only that
/// user's events.
#[implement(Service)]
#[tracing::instrument(level = "trace", skip(self))]
async fn next_due_of(&self, user_id: &UserId) -> Option<u64> {
	self.owned_ids(user_id)
		.broad_filter_map(async |delay_id| self.get_event(&delay_id).await.ok())
		.ready_fold(None, |next: Option<u64>, event| {
			Some(next.map_or(event.due_ms, |next| next.min(event.due_ms)))
		})
		.await
}

#[implement(Service)]
fn owned_ids<'a>(&'a self, user_id: &'a UserId) -> impl Stream<Item = DelayId> + Send + 'a {
	self.db
		.userid_delayid
		.keys_prefix(&(user_id, Interfix))
		.ignore_err()
		.map(|(_, delay_id): (Ignore, &str)| DelayId::from(delay_id))
}

#[implement(Service)]
#[tracing::instrument(level = "trace", skip(self))]
async fn get_event(&self, delay_id: &str) -> Result<Event> {
	self.db
		.delayid_event
		.get(delay_id)
		.await
		.deserialized()
		.map(|Json(event): Json<Event>| event)
}

/// Writes a new event's rows and its transaction response in one batch.
///
/// Committing the response with the rows means a retried request finds both
/// or neither.
#[implement(Service)]
fn insert(&self, delay_id: &str, event: &Event, response: Option<(RoomTxn<'_>, RoomScope<'_>)>) {
	let mut batch = self.services.db.txn();

	batch.raw_put(&self.db.delayid_event, delay_id, Json(event));
	batch.put_raw(&self.db.dueat_delayid, (event.due_ms, delay_id), []);
	batch.put_raw(&self.db.userid_delayid, (&event.user_id, delay_id), []);

	if let Some((txn, scope)) = response {
		self.services
			.transaction_ids
			.add_room_txnid_batch(&mut batch, txn, scope, delay_id.as_bytes());
	}

	batch.execute();
}

#[implement(Service)]
fn reschedule(&self, delay_id: &str, event: &Event, old_due: u64) {
	let mut batch = self.services.db.txn();

	batch.raw_put(&self.db.delayid_event, delay_id, Json(event));
	batch.del(&self.db.dueat_delayid, (old_due, delay_id));
	batch.put_raw(&self.db.dueat_delayid, (event.due_ms, delay_id), []);
	batch.execute();
}

#[implement(Service)]
fn remove(&self, delay_id: &str, user_id: &UserId, due: u64) {
	let mut batch = self.services.db.txn();

	batch.del_raw(&self.db.delayid_event, delay_id);
	batch.del(&self.db.dueat_delayid, (due, delay_id));
	batch.del(&self.db.userid_delayid, (user_id, delay_id));
	batch.execute();
}

#[implement(Service)]
fn drop_due(&self, delay_id: &str, due: u64) {
	let mut batch = self.services.db.txn();

	batch.del_raw(&self.db.delayid_event, delay_id);
	batch.del(&self.db.dueat_delayid, (due, delay_id));
	batch.execute();
}

#[implement(Service)]
fn drop_owned(&self, user_id: &UserId, delay_id: &str) {
	let mut batch = self.services.db.txn();

	batch.del_raw(&self.db.delayid_event, delay_id);
	batch.del(&self.db.userid_delayid, (user_id, delay_id));
	batch.execute();
}

fn limit_exceeded(retry_after: Option<Duration>) -> Error {
	let retry_after = retry_after.map(RetryAfter::Delay);
	let kind = ErrorKind::LimitExceeded(LimitExceededErrorData { retry_after });
	let message = "The maximum number of delayed events has been reached.";

	Error::Request(kind, message.into(), StatusCode::TOO_MANY_REQUESTS)
}
