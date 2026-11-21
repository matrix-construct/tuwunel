//! Follows delegated delayed leave events (MSC4195).
//!
//! A call member hands this server the delayed event that announces its
//! departure. The server polls LiveKit for the member's participant: once it
//! has connected, the event is restarted well before it falls due, and once
//! it is gone, the event is sent at once. A participant that never connects is
//! let go, and its event fires on its own time.

use std::time::{Duration, Instant};

use futures::{StreamExt, TryFutureExt};
use http::StatusCode;
use ruma::{
	OwnedUserId, RoomId, UserId, api::client::rtc::livekit::delegate_delayed_leave::v1::Request,
};
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use tuwunel_core::{
	Error, Result,
	arrayvec::ArrayString,
	debug, debug_warn, err, error, implement,
	result::NotFound,
	smallstr::SmallString,
	utils::{BoolExt, rand::string_array, stream::ReadyExt, time::now_millis},
	warn,
};
use tuwunel_database::{Deserialized, Json};

use super::{
	Conf, Service,
	names::{Name, identity, room_name},
	room_service::Presence,
};
use crate::delayed_events::{DelayId, Event};

// Random alphanumerics, about 95 bits.
pub(super) type Generation = ArrayString<GENERATION_LEN>;

// A WebSocket URL of a few dozen bytes.
type Url = SmallString<[u8; 48]>;

/// A LiveKit participant: the name of its room and its identity there.
///
/// The pair keys a delegation. It hashes MSC4195's uniqueness tuple of room,
/// slot, member and user, so a new delegation for the same call member lands
/// on the old one's key and replaces it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct Participant {
	room: Name,
	identity: Name,
}

/// A delegated delayed leave event, stored under its participant.
///
/// A field added later must default, as `connected` does: a record without it
/// reads as waiting. A poll acts only on the record exactly as it read it before asking
/// LiveKit, so an answer to an older question never applies to a delegation
/// that has changed since.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Record {
	/// The user who delegated the event, its owner.
	pub(super) user_id: OwnedUserId,

	/// The delegated event.
	pub(super) delay_id: DelayId,

	/// The SFU the delegation names, by its configured `url`.
	///
	/// Older records omit the field, and an empty url names no SFU.
	#[serde(default = "Url::new")]
	pub(super) url: Url,

	/// Tells this delegation apart from every other, one made again for the
	/// same event at the same moment included.
	pub(super) generation: Generation,

	/// When the delegation was made, as Unix time in milliseconds.
	pub(super) since_ms: u64,

	/// Whether LiveKit has reported the participant connected since then.
	#[serde(default)]
	pub(super) connected: bool,
}

/// What one poll does with a delegation.
///
/// Only `Send` ends the delayed event; after `Wait` or `GiveUp` it stays
/// scheduled and fires on its own time unless restarted.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum Action {
	/// Leave the delegation as it is.
	Wait,

	/// Record the participant connected, restarting the event if it is due.
	Connect {
		restart: bool,
	},

	/// Restart the delayed event.
	Restart,

	/// Let go of a participant that never connected.
	GiveUp,

	/// Send the event, since the participant left.
	Send,
}

const GENERATION_LEN: usize = 16;

/// Shortest delay a delegated event may have.
///
/// A connected participant's event is restarted once it falls due within half
/// its delay plus one poll interval, which leaves several polls to spare even
/// at this floor.
const MIN_DELAY_MS: u64 = 60_000;

/// Interval between the starts of two polls of every delegation.
///
/// A participant's departure is noticed within about this long, at one
/// RoomService call per delegated member each time.
const POLL_INTERVAL_MS: u64 = 10_000;

/// Longest wait for a delegated participant to connect.
///
/// The wait is the event's delay or an hour, whichever is shorter. It never
/// outlasts the delay, so nothing need be restarted while waiting.
const MAX_CONNECT_WAIT_MS: u64 = 3_600_000;

// Participants asked about at once, so one stalled RoomService call does not stall the others.
const POLL_WIDTH: usize = 8;

/// Delegates a call member's delayed leave event to this server.
///
/// An event that is absent, another user's, in another room or no longer
/// scheduled reads as an unknown `delay_id`. Its delay must be at least a
/// minute, and the `url` must name this server's SFU. A delegation replaces
/// any earlier one for the same call member, and one made again for the same
/// event on the same SFU keeps whether its participant was seen connected.
/// Membership is not checked, since scheduling the event was.
#[implement(Service)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		%sender_user,
	),
)]
pub async fn delegate(&self, sender_user: &UserId, request: &Request) -> Result {
	BoolExt::ok_or_else(self.is_own_url(&request.url), foreign_url)?;

	let participant =
		Participant::new(sender_user, &request.room_id, &request.slot_id, &request.member_id);

	// Checked under the lock, so a poll sending the event meanwhile is seen.
	let _lock = self.locks.lock(&participant).await;
	let delay_ms = self
		.scheduled(&request.delay_id, sender_user)
		.map_ok(|event| {
			event
				.filter(|event| event.room_id == request.room_id)
				.map(|event| event.delay_ms)
		})
		.await?
		.ok_or_else(unknown_delay)?;

	BoolExt::ok_or_else(delay_ms >= MIN_DELAY_MS, short_delay)?;

	// Replacing an unreadable record could lose a connection it recorded.
	let connected = self
		.try_delegation(&participant)
		.inspect_err(|error| error!(%error, "Failed to read a LiveKit delegation"))
		.map_err(|_| err!("Failed to read the existing delegation."))
		.await?
		.is_some_and(|record| {
			record.connected
				&& record.delay_id == request.delay_id
				&& record.url.as_str() == request.url
		});

	let record = Record {
		connected,
		..Record::new(sender_user, &request.delay_id, &request.url, now_millis())
	};

	self.put(&participant, &record);

	Ok(())
}

#[implement(Participant)]
fn new(user_id: &UserId, room_id: &RoomId, slot_id: &str, member_id: &str) -> Self {
	Self {
		room: room_name(room_id, slot_id),
		identity: identity(user_id, member_id),
	}
}

/// Starts a delegation of a user's event on an SFU, waiting for its
/// participant.
///
/// Each delegation draws a fresh generation, so one made again for the same
/// event in the same millisecond is still a different delegation.
#[implement(Record)]
pub(super) fn new(user_id: &UserId, delay_id: &str, url: &str, since_ms: u64) -> Self {
	Self {
		user_id: user_id.to_owned(),
		delay_id: delay_id.into(),
		url: url.into(),
		generation: string_array(),
		since_ms,
		connected: false,
	}
}

/// Polls every delegation once per interval, until shutdown.
///
/// A pass that outlasts the interval is followed at once by the next. One
/// that outlasts half the shortest delay is logged, since an event at that
/// delay could then fall due between two polls.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn delegation_loop(&self) {
	let server = &self.services.server;
	let interval = Duration::from_millis(POLL_INTERVAL_MS);
	let slow = Duration::from_millis(MIN_DELAY_MS / 2);

	while server.is_running() {
		let started = Instant::now();

		self.poll_delegations(now_millis()).await;

		let elapsed = started.elapsed();

		if elapsed > slow {
			warn!(?elapsed, "Polling LiveKit for delegated leave events is falling behind");
		}

		timeout(interval.saturating_sub(elapsed), server.until_shutdown())
			.await
			.ok();
	}
}

/// Runs one poll over every delegation, as of `now` in Unix milliseconds or
/// the moment of each decision, whichever is later.
///
/// Each delegation's participant is asked about once, a few at a time, and
/// the pass ends early at shutdown. The worker passes the current time; a
/// caller may pass a later one to act as a poll would then.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn poll_delegations(&self, now: u64) {
	let Ok(conf) = self.conf() else {
		return;
	};

	self.db
		.lkparticipant_delegation
		.keys()
		.ready_filter_map(Participant::from_key)
		.take_until(self.services.server.until_shutdown())
		.for_each_concurrent(POLL_WIDTH, async |participant| {
			self.step(conf, &participant, now).await;
		})
		.await;
}

#[implement(Participant)]
fn from_key(key: Result<(&str, &str)>) -> Option<Self> {
	let (room, identity) = key
		.inspect_err(|error| error!(%error, "Unreadable LiveKit participant key"))
		.ok()?;

	Self::from_names(room, identity)
}

/// Reads a participant from the room name and identity stored for it.
///
/// A room name or identity longer than a name's 43 bytes reads as none.
#[implement(Participant)]
pub(super) fn from_names(room: &str, identity: &str) -> Option<Self> {
	let participant = Self {
		room: Name::from(room).ok()?,
		identity: Name::from(identity).ok()?,
	};

	Some(participant)
}

/// Follows one delegation for one poll.
///
/// A delegation made on another SFU, or whose event has ended, is dropped
/// without asking LiveKit, and one whose event cannot be read is kept for the
/// next poll. LiveKit is asked with no lock held, so a new delegation never
/// waits on it; the delegation must then be unchanged under the lock, or the
/// answer is stale and dropped, and its event is read again before anything
/// is done.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self, conf))]
async fn step(&self, conf: &Conf, participant: &Participant, now: u64) {
	let Some(record) = self.delegation(participant).await else {
		return;
	};

	// This SFU cannot tell whether another's participant is still connected.
	if record.url.as_str() != conf.url {
		return self.end(participant, &record).await;
	}

	let event = match self
		.scheduled(&record.delay_id, &record.user_id)
		.await
	{
		| Ok(Some(event)) => event,
		| Ok(None) => return self.end(participant, &record).await,
		| Err(error) => return unreadable(&record, &error),
	};

	let presence = self
		.get_participant(conf, &participant.room, &participant.identity)
		.inspect_err(
			|error| debug_warn!(%error, "LiveKit did not report a delegated participant"),
		)
		.await
		.ok();

	let now = now.max(now_millis());

	// A fresher event can only fall due later, so waiting stays waiting.
	if decide(&record, presence, &event, now) == Action::Wait {
		return;
	}

	let _lock = self.locks.lock(participant).await;

	if !self.unchanged(participant, &record).await {
		return;
	}

	let event = match self
		.scheduled(&record.delay_id, &record.user_id)
		.await
	{
		| Ok(Some(event)) => event,
		| Ok(None) => return self.remove(participant),
		| Err(error) => return unreadable(&record, &error),
	};

	let action = decide(&record, presence, &event, now);

	self.perform(participant, record, action).await;
}

/// Decides what one poll does with a delegation, from what LiveKit reported.
///
/// LiveKit's answer is `None` when it could not be had, which neither sends
/// nor restarts anything. A waiting delegation becomes connected once its
/// participant is active, even past its wait, restarting the event then if it
/// is due; any other answer, `None` included, leaves it waiting, restarting
/// nothing since the member's client still does, until its wait is over and it
/// is let go. A connected delegation sends its event once the participant is
/// gone, and otherwise restarts it once it falls due within half its delay plus
/// one poll interval.
pub(super) fn decide(
	record: &Record,
	presence: Option<Presence>,
	event: &Event,
	now: u64,
) -> Action {
	let due = now
		.saturating_add(event.delay_ms / 2)
		.saturating_add(POLL_INTERVAL_MS)
		.ge(&event.due_ms);

	let waited = now
		.saturating_sub(record.since_ms)
		.ge(&event.delay_ms.min(MAX_CONNECT_WAIT_MS));

	match (record.connected, presence) {
		| (false, Some(Presence::Active)) => Action::Connect { restart: due },
		| (false, _) if waited => Action::GiveUp,
		| (true, Some(Presence::Gone)) => Action::Send,
		| (true, Some(_)) if due => Action::Restart,
		| _ => Action::Wait,
	}
}

/// Carries out a poll's decision.
///
/// The caller holds the delegation's lock, so no new delegation lands between
/// the decision and its writes.
#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
async fn perform(&self, participant: &Participant, record: Record, action: Action) {
	match action {
		| Action::Wait => {},
		| Action::Restart => self.restart(participant, &record).await,
		| Action::GiveUp => self.remove(participant),
		| Action::Send => self.send(participant, &record).await,
		| Action::Connect { restart } => {
			let record = Record { connected: true, ..record };

			self.put(participant, &record);

			if restart {
				self.restart(participant, &record).await;
			}
		},
	}
}

/// Restarts a delegation's event.
///
/// An event sent or cancelled meanwhile ends the delegation; any other
/// failure is logged and leaves it for the next poll.
#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
async fn restart(&self, participant: &Participant, record: &Record) {
	let restarted = self
		.services
		.delayed_events
		.restart(&record.delay_id, Some(&record.user_id))
		.await;

	match restarted {
		| Ok(()) => {},
		| Err(error) if ended(&error) => self.remove(participant),
		| Err(error) => {
			let user_id = &record.user_id;

			warn!(%user_id, %error, "Failed to restart a delegated leave event");
		},
	}
}

/// Sends a delegation's event, since its participant left, and drops the
/// delegation.
///
/// Sending finalises the event whatever the outcome, so the delegation ends
/// either way.
#[implement(Service)]
async fn send(&self, participant: &Participant, record: &Record) {
	let sent = self
		.services
		.delayed_events
		.send(&record.delay_id, Some(&record.user_id))
		.await;

	let user_id = &record.user_id;

	debug!(%user_id, ?sent, "Sent a delegated leave event");
	self.remove(participant);
}

/// Reads a user's delayed event while it is still scheduled.
///
/// An event missing, another user's or already ended reads as none; a fault
/// reading it is an error.
#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
async fn scheduled(&self, delay_id: &str, user_id: &UserId) -> Result<Option<Event>> {
	self.services
		.delayed_events
		.find(delay_id, Some(user_id))
		.map_ok(|event| event.filter(|event| event.finalised.is_none()))
		.await
}

#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
async fn end(&self, participant: &Participant, read: &Record) {
	let _lock = self.locks.lock(participant).await;

	// A new delegation may have replaced the one the poll read; that one stays.
	if self.unchanged(participant, read).await {
		self.remove(participant);
	}
}

#[implement(Service)]
async fn unchanged(&self, participant: &Participant, read: &Record) -> bool {
	self.delegation(participant)
		.await
		.is_some_and(|record| record == *read)
}

/// Reads a participant's delegation.
///
/// A missing one reads as none, and so does an unreadable one, which is
/// logged.
#[implement(Service)]
async fn delegation(&self, participant: &Participant) -> Option<Record> {
	self.try_delegation(participant)
		.inspect_err(|error| error!(?participant, %error, "Unreadable LiveKit delegation"))
		.await
		.ok()
		.flatten()
}

/// Reads a participant's stored delegation.
///
/// A missing one reads as none, and a fault reading or decoding it is an
/// error.
#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
async fn try_delegation(&self, participant: &Participant) -> Result<Option<Record>> {
	self.db
		.lkparticipant_delegation
		.qry(&participant.key())
		.await
		.deserialized()
		.map(|Json(record): Json<Record>| record)
		.present()
}

#[implement(Participant)]
#[inline]
pub(super) fn key(&self) -> (&str, &str) { (&self.room, &self.identity) }

#[implement(Service)]
fn put(&self, participant: &Participant, record: &Record) {
	self.db
		.lkparticipant_delegation
		.put(participant.key(), Json(record));
}

#[implement(Service)]
fn remove(&self, participant: &Participant) {
	self.db
		.lkparticipant_delegation
		.del(participant.key());
}

// A storage fault is not the event's end, so the delegation stays for the next poll.
fn unreadable(record: &Record, error: &Error) {
	let user_id = &record.user_id;

	error!(%user_id, %error, "Failed to read a delegated leave event");
}

// Gone, or sent or cancelled since the poll read it.
fn ended(error: &Error) -> bool {
	error.is_not_found() || error.status_code() == StatusCode::CONFLICT
}

// One answer for an ID that is missing, another user's, in another room, or ended.
fn unknown_delay() -> Error { err!(Request(InvalidParam("Unknown delay_id."))) }

fn short_delay() -> Error {
	err!(Request(InvalidParam("The delayed event's delay is too short to delegate.")))
}

fn foreign_url() -> Error {
	err!(Request(InvalidParam("Delegation is not offered for that url.")))
}
