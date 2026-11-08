use std::{
	collections::{BTreeMap, VecDeque},
	iter::empty,
	pin::pin,
};

use futures::{
	StreamExt, TryFutureExt, TryStreamExt,
	future::{join, try_join},
};
use ruma::{
	OwnedRoomId, RoomId, UserId,
	api::client::push::{EmailPusherData, Pusher, PusherKind},
	events::room::member::MembershipState,
	push::Action,
};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use tuwunel_core::{
	Error, Result, implement, is_not_equal_to,
	result::NotFound,
	utils::{
		BoolExt,
		future::OptionFutureExt,
		stream::{BroadbandExt, IterStream, ReadyExt, TryReadyExt, TryWidebandExt},
	},
	warn,
};
use tuwunel_database::{Cbor, Deserialized};
use tuwunel_matrix::{
	Event,
	pdu::{Pdu, PduId, RawPduId},
};

use super::render::SHOWN;
use crate::{
	pusher::Notified, rooms::short::ShortRoomId, sendmail::Address, threepid::canonicalize_email,
};

/// What one evaluation of an email pusher found.
///
/// The worker drops a sent or idle pusher from its schedule, evaluates a
/// pending one again at the time it carries, and retries a failed one with
/// backoff.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
	/// A mail went out, and no notification is still being written.
	Sent,

	/// Nothing is waiting, or the pusher can no longer be mailed.
	Idle,

	/// Notifications are waiting or still being written, and the pusher is
	/// evaluated again at this time in milliseconds.
	Pending(u64),

	/// The evaluation failed and nothing was recorded, so it is retried.
	Failed,
}

/// One room's mail throttle for one email pusher.
///
/// Only an evaluation that mails the room rewrites it; a later read of the room
/// resets it when it is next loaded.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Throttle {
	/// When the last mail naming the room went out, in milliseconds.
	pub(super) last_sent_ms: u64,

	/// How long after that mail the room stays quiet, in milliseconds.
	pub(super) period_ms: u64,

	/// The global count when that mail went out; a read of the room counted
	/// after it resets the throttle.
	pub(super) sent_count: u64,
}

pub(super) struct Pending {
	pub(super) ts: u64,
	pub(super) sroomid: ShortRoomId,
	pub(super) pdu: Pdu,
}

/// What a scan of a pusher's notification rows gathered.
///
/// It keeps only what the timing and the mail need, so it stays small however
/// many rows the scan reads.
#[derive(Default)]
pub(super) struct Scan {
	/// The last row read.
	pub(super) last: Option<u64>,

	/// The first row still to mail.
	pub(super) first: Option<u64>,

	/// Each room with a notification to mail, and when its earliest one
	/// arrived, which with the room's throttle decides when the room falls
	/// due.
	pub(super) rooms: PendingRooms,

	/// The newest notifications to mail, up to `SHOWN`, oldest first.
	pub(super) newest: VecDeque<Pending>,

	/// How many notifications there are to mail.
	pub(super) total: usize,
}

type PendingRooms = BTreeMap<ShortRoomId, (OwnedRoomId, u64)>;

type Throttles<'a> = BTreeMap<ShortRoomId, RoomThrottle<'a>>;

type RoomThrottle<'a> = (&'a RoomId, Throttle);

type StoredRow<'a> = ((&'a UserId, u64), Notified);

/// Factor a room's throttle grows by with each mail naming it.
///
/// With the default ten-minute delay, one step reaches the one-day cap, the
/// schedule Synapse uses.
const THROTTLE_MULTIPLIER: u64 = 144;

/// The longest a room's throttle grows, one day in milliseconds.
///
/// A first throttle longer than this, from a configured delay over a day, is
/// cut to it as well.
const THROTTLE_MAX_MS: u64 = 24 * 60 * 60 * 1000;

/// The soonest a pusher is evaluated again for a notification still being
/// written, in milliseconds.
///
/// Without a delay, it keeps the worker from spinning until the write lands.
const REVISIT_MIN_MS: u64 = 1000;

/// Evaluates one email pusher at `now`, in milliseconds.
///
/// A notification after the pusher's cursor counts if it still notifies, is
/// unread, and is in a room the user has joined or been invited to. Each waits
/// out the delay and its room's throttle; once the earliest is due, all of them
/// go out in one mail and the cursor moves past them. A notification still
/// being written brings the pusher back after the delay or one second,
/// whichever is longer.
#[implement(super::super::Service)]
#[tracing::instrument(level = "debug", skip(self, pushkey))]
pub async fn evaluate_email(&self, user_id: &UserId, pushkey: &str, now: u64) -> Outcome {
	let (address, data) = match self.email_target(user_id, pushkey).await {
		| Ok(target) => target,
		| Err(outcome) => return outcome,
	};

	let stored = match self.email_cursor(user_id, pushkey).await {
		| Ok(Some(cursor)) => cursor,
		| Ok(None) => return Outcome::Idle,
		| Err(_) => return Outcome::Failed,
	};

	let delay = self
		.services
		.config
		.push
		.email_delay
		.saturating_mul(1000);

	let frontier = self.services.globals.current_count();
	let Ok((scan, in_flight)) = try_join(
		self.scan_pending(user_id, stored, frontier),
		self.notified_past(user_id, frontier),
	)
	.await
	else {
		return Outcome::Failed;
	};

	let revisit = in_flight.then(|| now.saturating_add(delay.max(REVISIT_MIN_MS)));
	let nothing_due = revisit.map_or(Outcome::Idle, Outcome::Pending);
	let Some(last) = scan.last else {
		return nothing_due;
	};

	let Some(first) = scan.first else {
		self.record_state(user_id, pushkey, stored, last, empty())
			.await;

		return nothing_due;
	};

	let Ok(throttles) = self
		.room_throttles(user_id, pushkey, &scan.rooms)
		.await
	else {
		return Outcome::Failed;
	};

	let due = scan
		.rooms
		.iter()
		.map(|(sroomid, &(_, ts))| {
			let throttle = throttles
				.get(sroomid)
				.map(|&(_, throttle)| throttle)
				.unwrap_or_default();

			due_at(ts, throttle, delay)
		})
		.min()
		.unwrap_or(now);

	if due > now {
		let cursor = first.saturating_sub(1);

		if cursor > stored {
			self.record_state(user_id, pushkey, stored, cursor, empty())
				.await;
		}

		return Outcome::Pending(revisit.map_or(due, |revisit| due.min(revisit)));
	}

	let brand = data
		.data
		.get("brand")
		.and_then(JsonValue::as_str)
		.unwrap_or_else(|| self.services.globals.server_name().as_str());

	let mail = self
		.render_email(user_id, &address, brand, &scan)
		.await;

	let Ok(()) = self
		.services
		.sendmail
		.send_auto_generated(&address, &mail.subject, mail.text)
		.inspect_err(|error| warn!(%user_id, %error, "Failed to send a notification email"))
		.await
	else {
		return Outcome::Failed;
	};

	let sent_count = self.services.globals.current_count();
	let throttles = throttles
		.into_values()
		.map(|(room_id, throttle)| (room_id, sent_throttle(throttle, now, sent_count, delay)));

	self.record_state(user_id, pushkey, stored, last, throttles)
		.await;

	revisit.map_or(Outcome::Sent, Outcome::Pending)
}

/// The address an email pusher mails and the pusher's data, or the outcome
/// ending its evaluation.
///
/// A pusher that is gone, or no longer an email pusher, takes its rows with it.
/// One whose address is no longer verified on the account, or is not a valid
/// address, is deleted, as removing the address deletes it in Synapse.
#[implement(super::super::Service)]
async fn email_target(
	&self,
	user_id: &UserId,
	pushkey: &str,
) -> Result<(Address, EmailPusherData), Outcome> {
	let Some(data) = self
		.email_pusher(user_id, pushkey)
		.await
		.inspect_err(|error| warn!(%user_id, %error, "Failed to read an email pusher"))
		.map_err(|_| Outcome::Failed)?
	else {
		self.forget_orphan(user_id, pushkey).await;
		return Err(Outcome::Idle);
	};

	if !self.services.sendmail.is_enabled() {
		return Err(Outcome::Idle);
	}

	let bound = self
		.services
		.threepid
		.address_bound_to(user_id, pushkey)
		.await
		.inspect_err(|error| warn!(%user_id, %error, "Failed to read an email pusher's binding"))
		.map_err(|_| Outcome::Failed)?;

	let Some(address) = deliverable(bound, pushkey) else {
		self.forget_unbound(user_id, pushkey).await;
		return Err(Outcome::Idle);
	};

	Ok((address, data))
}

/// The pusher's email data, or `None` when it is gone or is not an email
/// pusher.
///
/// Failing to read the pusher is an error, never taken for its absence.
#[implement(super::super::Service)]
pub(super) async fn email_pusher(
	&self,
	user_id: &UserId,
	pushkey: &str,
) -> Result<Option<EmailPusherData>> {
	self.get_pusher(user_id, pushkey)
		.await
		.optional()
		.map(|pusher| pusher.and_then(email_data))
}

/// Deletes the cursor and room throttles of a pusher that is gone or no longer
/// mails.
///
/// The pusher is read again under its lock, so one created again since keeps
/// its new cursor.
#[implement(super::super::Service)]
async fn forget_orphan(&self, user_id: &UserId, pushkey: &str) {
	let _lock = self.email_lock(user_id, pushkey).await;

	if matches!(self.email_pusher(user_id, pushkey).await, Ok(None)) {
		self.forget_email_state(user_id, pushkey).await;
	}
}

/// Deletes an email pusher whose address is no longer verified on the account
/// or is not a valid address.
///
/// The pusher and its binding are read again under the pusher's lock, so a
/// pusher replaced in the meantime, or one whose address was verified again,
/// survives.
#[implement(super::super::Service)]
async fn forget_unbound(&self, user_id: &UserId, pushkey: &str) {
	let _lock = self.email_lock(user_id, pushkey).await;

	if !matches!(self.email_pusher(user_id, pushkey).await, Ok(Some(_))) {
		return;
	}

	self.services
		.threepid
		.address_bound_to(user_id, pushkey)
		.await
		.is_ok_and(|bound| deliverable(bound, pushkey).is_none())
		.then_async(|| self.remove_pusher(user_id, pushkey))
		.await;
}

/// Gathers the notifications to mail between the cursor and the retired
/// `frontier`.
///
/// A row past the frontier may sit above one still being written, so the scan
/// stops before it. It reads every row up to there, a backlog the throttle cap
/// keeps to about a day of notifications. A row, event or membership whose read
/// may succeed on a retry ends the scan with its error; one unreadable for good
/// is skipped.
#[implement(super::super::Service)]
async fn scan_pending(&self, user_id: &UserId, cursor: u64, frontier: u64) -> Result<Scan> {
	self.db
		.useridcount_notification
		.stream_from(&(user_id, cursor.saturating_add(1)))
		.ready_filter_map(|row| readable(user_id, cursor, row))
		.ready_try_take_while(|((user, count), _): &StoredRow<'_>| {
			Ok(*user == user_id && *count <= frontier)
		})
		.map_ok(|((_, count), notified): StoredRow<'_>| (count, notified))
		.wide_and_then(async |(count, notified)| {
			self.scanned(user_id, count, notified)
				.map_ok(|pending| (count, pending))
				.await
		})
		.ready_try_fold_default(|scan: Scan, row| Ok(gather(scan, row)))
		.await
}

/// The notification as a pending mail, or `None` once it no longer warrants
/// one.
///
/// It must still notify, survive unredacted, be unread, and be in a room the
/// user is still joined to, or invited to when the notification is that invite.
/// An event or membership whose read may succeed on a retry is an error; one
/// missing or unreadable for good is skipped.
#[implement(super::super::Service)]
async fn scanned(
	&self,
	user_id: &UserId,
	count: u64,
	notified: Notified,
) -> Result<Option<Pending>> {
	if !notified.actions.iter().any(Action::should_notify) {
		return Ok(None);
	}

	let pdu_id: RawPduId = PduId {
		shortroomid: notified.sroomid,
		count: count.into(),
	}
	.into();

	let Some(pdu) = self
		.services
		.timeline
		.get_pdu_from_id(&pdu_id)
		.await
		.present()
		// Kept at warn: nothing else in a stock build shows the unreadable event.
		.inspect_err(|error| warn!(%user_id, count, %error, "Failed to read a notified event"))
		.or_else(transient_or_none)?
		.filter(|pdu| !pdu.is_redacted())
	else {
		return Ok(None);
	};

	if self.notification_read(user_id, &pdu, count).await {
		return Ok(None);
	}

	let state_cache = &self.services.state_cache;
	let room_id = pdu.room_id();
	let member = invites(&pdu, user_id)
		.then_async(|| state_cache.try_is_invited(user_id, room_id))
		.unwrap_or_else_async(|| state_cache.try_is_joined(user_id, room_id))
		.map_ok(Some)
		.inspect_err(|error| warn!(%user_id, %room_id, %error, "Failed to read a membership"))
		.await
		.or_else(transient_or_none)?
		.unwrap_or_default();

	let pending = Pending {
		ts: notified.ts,
		sroomid: notified.sroomid,
		pdu,
	};

	Ok(member.then_some(pending))
}

/// Whether the user has a notification row past the retired `frontier`.
///
/// Such a row may sit above one still being written, which a later evaluation
/// must not skip. A row whose read may succeed on a retry is an error, and one
/// unreadable for good is skipped.
#[implement(super::super::Service)]
async fn notified_past(&self, user_id: &UserId, frontier: u64) -> Result<bool> {
	let past = self
		.db
		.useridcount_notification
		.keys_from(&(user_id, frontier.saturating_add(1)))
		.ready_filter_map(|key| readable(user_id, frontier, key));

	pin!(past)
		.next()
		.await
		.transpose()
		.map(|first| first.is_some_and(|(user, _): (&UserId, u64)| user == user_id))
}

/// Each pending room's throttle as it applies now.
///
/// A room the user has read since the last mail naming it has its throttle
/// reset. A read that may succeed on a retry returns an error, while a throttle
/// unreadable for good counts as none.
#[implement(super::super::Service)]
async fn room_throttles<'a>(
	&self,
	user_id: &UserId,
	pushkey: &str,
	rooms: &'a PendingRooms,
) -> Result<Throttles<'a>> {
	rooms
		.iter()
		.stream()
		.broad_then(|(&sroomid, (room_id, _))| {
			self.room_throttle(user_id, pushkey, sroomid, room_id)
		})
		.try_collect()
		.await
}

#[implement(super::super::Service)]
async fn room_throttle<'a>(
	&self,
	user_id: &UserId,
	pushkey: &str,
	sroomid: ShortRoomId,
	room_id: &'a RoomId,
) -> Result<(ShortRoomId, RoomThrottle<'a>)> {
	let stored = self
		.db
		.senderkeyroom_emailthrottle
		.qry(&(user_id, pushkey, room_id));

	let last_read = self.last_notification_read(user_id, room_id);
	let (stored, last_read) = join(stored, last_read).await;
	let stored = stored
		.deserialized()
		.map(|Cbor(stored): Cbor<Throttle>| stored)
		.present()
		.inspect_err(
			|error| warn!(%user_id, %room_id, %error, "Failed to read an email throttle"),
		)
		.or_else(transient_or_none)?
		.unwrap_or_default();

	let last_read = last_read
		.optional()
		.inspect_err(
			|error| warn!(%user_id, %room_id, %error, "Failed to read a room's last read"),
		)
		.ok()
		.flatten();

	Ok((sroomid, (room_id, effective_throttle(stored, last_read))))
}

/// Moves the pusher's cursor on from `stored` and stores the throttles of the
/// rooms a mail named, unless the cursor has changed since the evaluation read
/// it.
///
/// Deleting the pusher, or creating it again, changes the cursor, so a result
/// computed from the old one is dropped; the lock keeps either from landing
/// between the check and the write. A cursor that cannot be read counts as
/// unchanged: dropping the result would mail the same notifications again,
/// while writing it at worst leaves a deleted pusher's cursor for its next
/// evaluation to delete, or sets a pusher created since back to this cursor,
/// so it also mails what came before it to the same address.
#[implement(super::super::Service)]
async fn record_state<'a, Rooms>(
	&self,
	user_id: &UserId,
	pushkey: &str,
	stored: u64,
	cursor: u64,
	rooms: Rooms,
) where
	Rooms: Iterator<Item = RoomThrottle<'a>> + Send,
{
	let _lock = self.email_lock(user_id, pushkey).await;
	let changed = self
		.email_cursor(user_id, pushkey)
		.await
		.is_ok_and(is_not_equal_to!(Some(stored)));

	if !changed {
		self.write_state(user_id, pushkey, cursor, rooms);
	}
}

/// Writes the cursor and each room's throttle in one transaction.
///
/// The cursor never moves past a mail without the throttles it set.
#[implement(super::super::Service)]
fn write_state<'a, Rooms>(&self, user_id: &UserId, pushkey: &str, cursor: u64, rooms: Rooms)
where
	Rooms: Iterator<Item = RoomThrottle<'a>>,
{
	let mut txn = self.db.db.txn();

	txn.put(&self.db.senderkey_emailcursor, (user_id, pushkey), cursor);
	rooms.for_each(|(room_id, throttle)| {
		let key = (user_id, pushkey, room_id);

		txn.put(&self.db.senderkeyroom_emailthrottle, key, Cbor(throttle));
	});

	txn.execute();
}

/// The address a pushkey is mailed at: its canonical form, the one the binding
/// was verified under.
///
/// It is `None` when the address is not bound to the account or is not a valid
/// address. Mailing the client's spelling instead could reach another mailbox
/// whose address folds to the same canonical form.
fn deliverable(bound: bool, pushkey: &str) -> Option<Address> {
	bound.and_then(|| Address::try_from(canonicalize_email(pushkey).ok()?).ok())
}

fn email_data(pusher: Pusher) -> Option<EmailPusherData> {
	match pusher.kind {
		| PusherKind::Email(data) => Some(data),
		| _ => None,
	}
}

/// A notification row or key read past `after`, or its error when a retry may
/// read it.
///
/// Every failed read is logged; one unreadable for good is dropped.
fn readable<T>(user_id: &UserId, after: u64, read: Result<T>) -> Option<Result<T>> {
	read.map(Some)
		// Kept at warn: nothing else in a stock build shows the unreadable row.
		.inspect_err(|error| warn!(%user_id, after, %error, "Failed to read a notification"))
		.or_else(transient_or_none)
		.transpose()
}

/// The error when a retry may succeed, else `None`, which skips what failed.
///
/// Only an I/O error of a transient kind counts; corruption and decode errors
/// do not.
fn transient_or_none<T>(error: Error) -> Result<Option<T>> {
	error
		.is_transient_io()
		.then_some(error)
		.map_or(Ok(None), Err)
}

pub(super) fn invites(pdu: &Pdu, user_id: &UserId) -> bool {
	pdu.membership_for(user_id) == Some(MembershipState::Invite)
}

/// Folds one scanned row, and the notification it leaves to mail, into the
/// scan.
///
/// Rows arrive in count order, so a room's first notification is its earliest.
pub(super) fn gather(mut scan: Scan, (count, pending): (u64, Option<Pending>)) -> Scan {
	scan.last = Some(count);
	pending
		.into_iter()
		.fold(scan, |scan, pending| keep(scan, count, pending))
}

fn keep(mut scan: Scan, count: u64, pending: Pending) -> Scan {
	scan.first.get_or_insert(count);
	scan.total = scan.total.saturating_add(1);
	scan.rooms
		.entry(pending.sroomid)
		.or_insert_with(|| (pending.pdu.room_id().to_owned(), pending.ts));

	scan.newest.push_back(pending);
	if scan.newest.len() > SHOWN {
		scan.newest.pop_front();
	}

	scan
}

/// A stored throttle as it applies now.
///
/// A read of the room counted after the last mail naming it zeroes the
/// throttle; the stored row is left as it is.
pub(super) fn effective_throttle(stored: Throttle, last_read: Option<u64>) -> Throttle {
	let read_since = last_read.is_some_and(|last_read| last_read > stored.sent_count);

	Throttle {
		period_ms: read_since.copy_or(stored.period_ms, 0),
		..stored
	}
}

/// The earliest a notification that arrived at `ts` may be mailed.
///
/// Both the delay and its room's throttle, counted from the last mail naming
/// the room, must have run out. Without a delay no room is quieted, whatever
/// throttle an earlier delay left it.
pub(super) fn due_at(ts: u64, throttle: Throttle, delay: u64) -> u64 {
	let quiet_until = throttle
		.last_sent_ms
		.saturating_add(throttle.period_ms);

	match delay {
		| 0 => ts,
		| _ => ts.saturating_add(delay).max(quiet_until),
	}
}

/// A room's throttle once a mail naming it has gone out.
///
/// A room without one starts at the delay, then each mail multiplies it, up to
/// a day; reading the room starts it over, and without a delay it stays at
/// none. `sent_count` is the global count a later read is compared with.
pub(super) fn sent_throttle(
	effective: Throttle,
	now: u64,
	sent_count: u64,
	delay: u64,
) -> Throttle {
	let period_ms = match (delay, effective.period_ms) {
		| (0, _) | (_, 0) => delay,
		| (_, period_ms) => period_ms.saturating_mul(THROTTLE_MULTIPLIER),
	};

	Throttle {
		last_sent_ms: now,
		period_ms: period_ms.min(THROTTLE_MAX_MS),
		sent_count,
	}
}
