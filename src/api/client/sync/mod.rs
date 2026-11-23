mod profiles;
#[cfg(test)]
mod tests;
mod v3;
mod v5;

use std::collections::VecDeque;

use futures::{StreamExt, pin_mut, stream::iter};
use ruma::{
	EventId, OwnedUserId, RoomId, UserId,
	events::{
		AnyStrippedStateEvent, AnySyncStateEvent, StateEventType,
		TimelineEventType::{RoomCreate, RoomMember},
		room::member::MembershipState,
	},
	serde::Raw,
	uint,
};
use tuwunel_core::{
	Error, Result, async_noinline, at, debug_warn, implement, is_equal_to,
	utils::{
		BoolExt,
		result::{LogErr, NotFound},
		stream::{IterStream, ReadyExt, TryReadyExt, WidebandExt},
		time::now_millis,
	},
	warn,
};
use tuwunel_matrix::{
	Event, PduCount,
	pdu::{EventHash, PduEvent},
};
use tuwunel_service::{Services, users::InviteFilter};

pub use self::v3::calculate_heroes;
pub(crate) use self::{v3::sync_events_route, v5::sync_events_v5_route};
use crate::utils::is_ignored_sender;

/// One raw row as the timeline scan judged it.
///
/// The outer error is a row that could not be read; the inner result is the
/// visibility decision, or the visibility read that failed.
type Judged = Result<(PduCount, PduEvent, Result<bool>)>;

/// Events a timeline scan may judge per event it can return, after Synapse's
/// filtered-recents loop (five passes over twice the limit).
const SCAN_FACTOR: usize = 10;

/// The fewest events a timeline scan may judge, after the same loop.
const SCAN_FLOOR: usize = 50;

#[derive(Clone, Copy)]
enum TimelineErrors {
	Ignore,
	Propagate,
}

#[derive(Clone, Copy)]
struct TimelineQuery<'a> {
	sender_user: &'a UserId,
	room_id: &'a RoomId,

	/// The client's position; the window holds only later events.
	since: PduCount,

	/// The window's last position, inclusive.
	next_batch: Option<PduCount>,

	/// The most events the window returns.
	limit: usize,
}

/// A room's timeline window, oldest event first.
///
/// A window left empty by events the user may not see can still carry the
/// position where backward pagination resumes past them.
#[derive(Default)]
struct TimelineWindow {
	/// The events the user may see.
	pdus: Vec<(PduCount, PduEvent)>,

	/// Whether events before the first were left out.
	limited: bool,

	/// The newest event's position, whether or not the user may see it.
	last_count: PduCount,

	/// Where backward pagination resumes once the scan stopped early or kept
	/// nothing.
	resume: Option<PduCount>,
}

struct Scan {
	/// Events kept so far, oldest first.
	kept: VecDeque<(PduCount, PduEvent)>,

	/// How many rows were judged, unreadable ones included.
	judged: usize,

	/// The oldest position judged.
	oldest: Option<PduCount>,

	/// Whether the scan stopped before the window's start.
	limited: bool,

	/// The most events to keep.
	limit: usize,

	/// The most rows to judge.
	budget: usize,
}

enum Halt {
	/// The window filled, the budget ran out, or a hidden event changed state.
	Stopped(Scan),

	/// A required read failed.
	Failed(Error),
}

async fn load_timeline(services: &Services, query: TimelineQuery<'_>) -> Result<TimelineWindow> {
	load_timeline_with_errors(services, query, TimelineErrors::Ignore).await
}

async fn load_timeline_fallible(
	services: &Services,
	query: TimelineQuery<'_>,
) -> Result<TimelineWindow> {
	load_timeline_with_errors(services, query, TimelineErrors::Propagate).await
}

/// Loads the newest events in a room's window that the user may see.
///
/// The scan runs newest first and judges the rows of one window, with the row
/// past it, at a time concurrently. Its budget is the greater of fifty rows and
/// ten per event the window may keep, and a zero limit judges none. A hidden
/// event is left out, and one that changes state ends the scan, so the window
/// starts after it and the state section covers it. A failed read of the
/// newest row fails the load, as does any other failed read on a row the
/// window could keep when errors propagate; otherwise the row is skipped or
/// the event left out.
// Unspanned: span frames on this path overflow the debug-build worker stack.
// size firewall
#[async_noinline]
async fn load_timeline_with_errors<'a>(
	services: &'a Services,
	TimelineQuery {
		sender_user,
		room_id,
		since,
		next_batch,
		limit,
	}: TimelineQuery<'a>,
	errors: TimelineErrors,
) -> Result<TimelineWindow> {
	let until = next_batch.map(|count| count.saturating_add(1));
	let rows = services
		.timeline
		.pdus_rev(Some(sender_user), room_id, until);

	pin_mut!(rows);
	let newest = rows.next().await.transpose()?;
	let last_count = newest
		.as_ref()
		.map(at!(0))
		.filter(|count| matches!(count, PduCount::Normal(_)))
		.unwrap_or_else(PduCount::max);

	if limit == 0 {
		let limited = newest
			.as_ref()
			.is_some_and(|(count, _)| *count > since);

		return Ok(TimelineWindow {
			limited,
			last_count,
			..Default::default()
		});
	}

	let budget = limit.saturating_mul(SCAN_FACTOR).max(SCAN_FLOOR);
	let scan = Scan {
		kept: VecDeque::new(),
		judged: 0,
		oldest: None,
		limited: false,
		limit,
		budget,
	};

	// Chunks bound how far the concurrent judging reads ahead of where the scan stops.
	let scan = iter(newest.map(Ok))
		.chain(rows)
		.ready_try_take_while(|(count, _)| Ok(*count > since))
		.take(budget.saturating_add(1))
		.enumerate()
		.chunks(limit.saturating_add(1))
		.map(|chunk| {
			chunk
				.into_iter()
				.stream()
				.wide_then(|(index, row)| judge(services, sender_user, row, index < budget))
		})
		.flatten()
		.map(Ok)
		.ready_try_fold(scan, |scan, judged| scan.admit()?.record(judged, errors))
		.await
		.or_else(Halt::into_scan)?;

	Ok(scan.into_window(last_count))
}

/// Decides whether the user may see one raw row's event.
///
/// Within the scan budget, the user's own leave or ban is visible whenever the
/// scan reaches it. The row past the budget only shows that more history
/// exists, so it is decoded but not judged, and the scan stops before reading
/// its decision.
// Unspanned: span frames on this path overflow the debug-build worker stack.
async fn judge(
	services: &Services,
	sender_user: &UserId,
	row: Result<(PduCount, PduEvent)>,
	within_budget: bool,
) -> Judged {
	let (count, pdu) = row?;

	if !within_budget {
		return Ok((count, pdu, Ok(false)));
	}

	let visible = match pdu.membership_for(sender_user) {
		| Some(MembershipState::Leave | MembershipState::Ban) => Ok(true),
		| _ =>
			services
				.state_accessor
				.try_user_can_see_event(sender_user, &pdu)
				.await,
	};

	Ok((count, pdu, visible))
}

#[implement(Scan)]
fn admit(self) -> Result<Scan, Halt> {
	if self.kept.len() < self.limit && self.judged < self.budget {
		return Ok(self);
	}

	Err(self.stop())
}

/// Keeps a visible event, and ends the scan at a hidden one that changes state.
///
/// A failed read fails the scan when errors propagate; otherwise an unreadable
/// row is skipped and an event whose visibility cannot be read is left out with
/// a warning.
#[implement(Scan)]
fn record(self, judged: Judged, errors: TimelineErrors) -> Result<Scan, Halt> {
	let propagate = matches!(errors, TimelineErrors::Propagate);
	let (count, pdu, visible) = match judged {
		| Ok(row) => row,
		| Err(error) if propagate => return Err(Halt::Failed(error)),
		// The newest row is read before the scan, so a resume point is already set.
		| Err(_) => return Ok(self.counted(None, None)),
	};

	let visible = match visible {
		| Ok(visible) => visible,
		| Err(error) if propagate => return Err(Halt::Failed(error)),
		| Err(error) => {
			warn!(
				event_id = %pdu.event_id(),
				%error,
				"Failed to read the event's visibility; leaving it out of the timeline."
			);

			false
		},
	};

	let boundary = !visible && pdu.state_key().is_some();
	let scan = self.counted(Some(count), visible.then_some((count, pdu)));

	if boundary {
		return Err(scan.stop());
	}

	Ok(scan)
}

#[implement(Scan)]
fn counted(self, position: Option<PduCount>, kept: Option<(PduCount, PduEvent)>) -> Scan {
	let scan = self.keep(kept);

	Scan {
		judged: scan.judged.saturating_add(1),
		oldest: position.or(scan.oldest),
		..scan
	}
}

#[implement(Scan)]
fn keep(mut self, kept: Option<(PduCount, PduEvent)>) -> Scan {
	if let Some(kept) = kept {
		self.kept.push_front(kept);
	}

	self
}

#[implement(Scan)]
fn stop(self) -> Halt { Halt::Stopped(Scan { limited: true, ..self }) }

/// Returns the kept events oldest first.
///
/// When the scan stopped early or kept nothing, the oldest position it judged
/// is the resume point, so pagination still moves past the events left out.
#[implement(Scan)]
fn into_window(self, last_count: PduCount) -> TimelineWindow {
	let resume = self
		.oldest
		.filter(|_| self.limited || self.kept.is_empty());

	TimelineWindow {
		pdus: self.kept.into(),
		limited: self.limited,
		last_count,
		resume,
	}
}

#[implement(Halt)]
fn into_scan(self) -> Result<Scan> {
	match self {
		| Halt::Stopped(scan) => Ok(scan),
		| Halt::Failed(error) => Err(error),
	}
}

/// Returns the backward pagination token for a timeline slice.
///
/// A slice beginning at room creation has nothing before it, so its window's
/// end lets a members query describe the room as received.
/// Backward pagination from that token re-reads the slice once before ending.
/// An empty slice resumes below the oldest row its scan judged, if any.
fn timeline_prev_batch(
	timeline_pdus: &[(PduCount, PduEvent)],
	window_end: PduCount,
	resume: Option<PduCount>,
) -> Option<PduCount> {
	timeline_pdus
		.first()
		.map(|(count, pdu)| match pdu.kind() {
			| RoomCreate => window_end,
			| _ => *count,
		})
		.or(resume)
}

async fn share_encrypted_room(
	services: &Services,
	sender_user: &UserId,
	user_id: &UserId,
) -> bool {
	services
		.state_cache
		.get_shared_rooms(sender_user, user_id)
		.map(ToOwned::to_owned)
		.any(async |room_id| {
			services
				.state_accessor
				.is_encrypted_room(&room_id)
				.await
		})
		.await
}

/// Whether a user's join to a room is fresh since a sync token.
///
/// The joined count moves on every join and forced state replay except a local
/// user's profile change over the position its join recorded, which keeps it.
/// An unreadable count reads as unmoved.
#[tracing::instrument(level = "trace", skip(services))]
async fn joined_since(
	services: &Services,
	room_id: &RoomId,
	user_id: &UserId,
	since: u64,
) -> bool {
	services
		.state_cache
		.get_joined_count(room_id, user_id)
		.await
		.inspect_err(|error| debug_warn!(%room_id, %user_id, %error, "joined count unreadable"))
		.is_ok_and(|recorded| recorded > since)
}

/// The room's unexpired sticky events after `since` through `next_batch`, in
/// timeline order (MSC4354).
///
/// A sender the user ignores, or one on a server this one forbids, is left out
/// whatever the event type, and each event is readied for the user as a
/// timeline event is, its bundled aggregations included. An event the user's
/// view prunes, as an erased sender's is, has lost its stickiness and is left
/// out. A room whose short id cannot be read offers none, logged.
#[tracing::instrument(level = "trace", skip_all)]
async fn load_sticky_events(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	since: u64,
	next_batch: u64,
	now: u64,
) -> Vec<(PduCount, PduEvent)> {
	let shortroomid = services
		.short
		.get_shortroomid(room_id)
		.await
		.inspect_err(|error| warn!(%room_id, %error, "Unreadable room for sticky events"));

	let Ok(shortroomid) = shortroomid else {
		return Vec::new();
	};

	services
		.sticky
		.unexpired(shortroomid, since, next_batch, now)
		.wide_filter_map(async |(count, pdu)| {
			is_ignored_sender(services, pdu.sender(), sender_user)
				.await
				.is_false()
				.then(|| (count, pdu.served_to(Some(sender_user))))
		})
		.wide_then(async |(count, pdu)| {
			let pdu = services
				.pdu_metadata
				.bundle_aggregations(sender_user, pdu)
				.await;

			(count, pdu)
		})
		.ready_filter(|(_, pdu)| pdu.sticky_until(now).is_some())
		.collect()
		.await
}

/// MSC4155: whether a stored invite may be served to the invitee.
///
/// The verdict is the recipient's own, judged against the sender of the
/// stripped invite membership event. Unreadable invite state takes the same
/// verdict as the sender-less case below, since neither can name a sender.
async fn invite_permitted_room(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	filter: &InviteFilter,
) -> bool {
	filter.is_permissive()
		|| services
			.state_cache
			.invite_state(user_id, room_id)
			.await
			.map_or_else(
				|error| {
					debug_warn!(%user_id, %room_id, ?error, "invite state is unreadable; skipping the sender rules");
					filter.permits(None)
				},
				|invite_state| invite_permitted(user_id, room_id, filter, &invite_state),
			)
}

/// [`invite_permitted_room`] for a room whose stripped state is in hand.
///
/// Callers walking stored invites already hold the state and take this form,
/// which spares them the load the room-keyed form pays per room. `room_id`
/// names the affected row in the sender-less diagnostic and does not enter the
/// verdict. That diagnostic stays at debug on a release build deliberately,
/// since the condition repeats for every sync an affected user makes.
fn invite_permitted(
	user_id: &UserId,
	room_id: &RoomId,
	filter: &InviteFilter,
	invite_state: &[Raw<AnyStrippedStateEvent>],
) -> bool {
	// Load-bearing: keeps a permissive user off the sender derivation below.
	if filter.is_permissive() {
		return true;
	}

	let sender = invite_sender(user_id, invite_state);

	if sender.is_none() {
		debug_warn!(%user_id, %room_id, "invite state names no sender; skipping the sender rules");
	}

	filter.permits(sender.as_deref())
}

/// The sender of the stripped membership event inviting `user_id`.
///
/// The last matching entry wins. An invite this server recorded after the
/// federation route began sanitising stripped state holds one entry for this
/// cell, our own copy of the signed membership PDU, whose sender the origin
/// check authenticated. An invite stored before that still carries whatever
/// the inviting server sent ahead of our copy, and the array has no ordering
/// semantics in the spec, so reading the last entry is what keeps those
/// answering with the authenticated sender too.
fn invite_sender(
	user_id: &UserId,
	invite_state: &[Raw<AnyStrippedStateEvent>],
) -> Option<OwnedUserId> {
	invite_state
		.iter()
		.rev()
		.filter(|event| {
			event
				.get_field::<&str>("state_key")
				.is_ok_and(|state_key| state_key.is_some_and(is_equal_to!(user_id.as_str())))
		})
		.filter_map(|event| event.deserialize().ok())
		.find_map(|event| match event {
			| AnyStrippedStateEvent::RoomMember(member) if member.state_key == user_id =>
				Some(member.sender),
			| _ => None,
		})
}

/// Strips the unsigned fields a state section does not serve.
///
/// Fields private to the sender go unless the requester sent the event, and
/// stored aggregations always go, since state events receive none. The stored
/// `prev_content`/`prev_sender` pair goes too (Synapse injects it on timeline
/// fetches only), except on the requester's own membership and on events
/// duplicated from the returned timeline (MSC4222, full_state), the copies
/// clients read membership transitions from.
fn strip_state_unsigned(
	mut pdu: PduEvent,
	sender_user: &UserId,
	in_timeline: impl Fn(&PduEvent) -> bool,
) -> PduEvent {
	let own_membership =
		*pdu.kind() == RoomMember && pdu.state_key() == Some(sender_user.as_str());

	pdu.remove_private_unless_sender(Some(sender_user));
	pdu.remove_aggregations();

	if !own_membership && !in_timeline(&pdu) {
		pdu.remove_prev_state().log_err().ok();
	}

	pdu
}

/// The user's own departure in a left room's state, when it holds one.
///
/// A ban counts, since the membership indexes file a banned user as left. Any
/// other membership yields `None`, as a room this server left still holds the
/// invite a later rescission withdrew. A failed read is logged and yields `None`
/// too, so the client still learns of the departure. A kick or ban is sent by
/// someone else, so fields private to the sender are removed unless the user
/// sent it.
async fn own_departure(
	services: &Services,
	room_id: &RoomId,
	user_id: &UserId,
) -> Option<PduEvent> {
	services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomMember, user_id.as_str())
		.await
		.optional()
		.inspect_err(|error| warn!(%room_id, %user_id, %error, "Unreadable own departure event"))
		.ok()
		.flatten()
		.filter(|event| is_departure(event, user_id))
		.map(|event| event.served_unfolded_to(Some(user_id)))
}

fn is_departure(event: &PduEvent, user_id: &UserId) -> bool {
	matches!(
		event.membership_for(user_id),
		Some(MembershipState::Leave | MembershipState::Ban)
	)
}

/// Builds a minimal leave event for a left room whose state supplies none.
///
/// A rescinded or rejected federated invite, or deleted, missing, or broken
/// room state, leaves no real membership event to send, while clients read the
/// departure from one. The stand-in names the user as sender under a fresh
/// local event ID.
fn stand_in_leave(
	services: &Services,
	room_id: &RoomId,
	sender_user: &UserId,
) -> Result<Raw<AnySyncStateEvent>> {
	let event = PduEvent {
		event_id: EventId::new_v1(services.globals.server_name()),
		origin_server_ts: now_millis().try_into()?,
		kind: RoomMember,
		state_key: Some(sender_user.as_str().into()),
		sender: sender_user.to_owned(),
		content: serde_json::from_str(r#"{"membership":"leave"}"#)?,
		// The following keys are dropped on conversion
		room_id: room_id.to_owned(),
		depth: uint!(1),
		origin: None,
		unsigned: None,
		sticky: None,
		redacts: None,
		hashes: EventHash::default(),
		auth_events: Default::default(),
		prev_events: Default::default(),
	};

	Ok(event.into_format())
}
