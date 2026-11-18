//! Evaluates user visibility and event-authoring permissions.
//!
//! Historical visibility checks combine event-time state with retained
//! membership metadata. Invite and tombstone checks use the normal event-build
//! pipeline as non-persisting authorization probes.

use futures::{TryFutureExt, future::OptionFuture};
use ruma::{
	EventId, RoomId, UserId,
	events::{
		StateEventType, TimelineEventType,
		room::{
			history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
			member::{MembershipState, RoomMemberEventContent},
			tombstone::RoomTombstoneEventContent,
		},
	},
};
use tuwunel_core::{
	Err, Result, err, implement,
	utils::{
		BoolExt, FutureBoolExt, OptionExt, future::TryExtExt, result::NotFound, time::now_millis,
	},
};
use tuwunel_matrix::{Event, Pdu, PduCount, RawPduId, StateKey, pdu::PduBuilder};

use crate::rooms::{short::ShortStateHash, state::RoomMutexGuard, timeline::Mapped};

/// Bound the cost of locating a retained departure after later membership changes.
const MEMBERSHIP_STEPS: usize = 16;

/// The state an event's visibility is judged against.
///
/// Only the event's own recorded state lets a change of history visibility also
/// be judged under the value it sets.
#[derive(Clone, Copy)]
struct Snapshot {
	shortstatehash: ShortStateHash,

	/// The event's own recorded state, not the room's current state standing in.
	recorded: bool,
}

/// Reports whether a user may redact an event in a room.
///
/// Cross-room targets are denied, while create and server-ACL targets return a
/// forbidden error. Federation permits the own-event rule for any sender on
/// the target sender's server; power-level failures fall back to create-event
/// ownership and exact sender equality.
#[implement(super::Service)]
pub async fn user_can_redact(
	&self,
	redacts: &EventId,
	sender: &UserId,
	room_id: &RoomId,
	federation: bool,
) -> Result<bool> {
	let redacting_event = self.services.timeline.get_pdu(redacts).await;

	if redacting_event
		.as_ref()
		.is_ok_and(|pdu| pdu.room_id() != room_id)
	{
		return Ok(false);
	}

	if redacting_event
		.as_ref()
		.is_ok_and(|pdu| *pdu.kind() == TimelineEventType::RoomCreate)
	{
		return Err!(Request(Forbidden("Redacting m.room.create is not safe, forbidding.")));
	}

	if redacting_event
		.as_ref()
		.is_ok_and(|pdu| *pdu.kind() == TimelineEventType::RoomServerAcl)
	{
		return Err!(Request(Forbidden(
			"Redacting m.room.server_acl will result in the room being inaccessible for \
			 everyone (empty allow key), forbidding."
		)));
	}

	match self.get_power_levels(room_id).await {
		| Ok(power_levels) => Ok(power_levels.user_can_redact_event_of_other(sender)
			|| power_levels.user_can_redact_own_event(sender)
				&& match redacting_event {
					| Ok(redacting_event) =>
						if federation {
							redacting_event.sender().server_name() == sender.server_name()
						} else {
							redacting_event.sender() == sender
						},
					| _ => false,
				}),
		| _ => {
			// Falling back on m.room.create to judge power level
			match self
				.room_state_get(room_id, &StateEventType::RoomCreate, "")
				.await
			{
				| Ok(room_create) => Ok(room_create.sender() == sender
					|| redacting_event
						.as_ref()
						.is_ok_and(|redacting_event| redacting_event.sender() == sender)),
				| _ => Err!(Database(
					"No m.room.power_levels or m.room.create events in database for room"
				)),
			}
		},
	}
}

/// Reports whether a user may see an event under its historical visibility.
///
/// An event without recorded state, such as a room's create event or a
/// backfilled event, is judged by the room's current state instead; it is
/// hidden when the room has none, as it is when the event's state cannot be
/// read. Missing or invalid history visibility defaults to `shared`, whose
/// decision also accounts for the user's membership intervals around the
/// event. Per the spec's before-or-after rule, the user's own membership event
/// is visible under `joined` and `invited` when the membership it sets
/// qualifies, and a change of history visibility is visible when either the
/// value before it or the value it sets allows the user.
///
/// An accepted, unexpired sticky event (MSC4354) is visible to a user joined to
/// the room whatever its history visibility, and even when the event's state
/// fails to read, though not when the room has no state at all.
#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "trace")]
pub async fn user_can_see_event<Pdu>(&self, user_id: &UserId, pdu: &Pdu) -> bool
where
	Pdu: Event,
{
	let visible = match self.event_snapshot(pdu).await {
		| Ok(None) => return false,
		| Err(error) => Err(error),
		| Ok(Some(snapshot)) => {
			let history_visibility = self
				.history_visibility(snapshot.shortstatehash)
				.await;

			let before = self
				.visible_at(user_id, pdu, snapshot.shortstatehash, Ok(history_visibility))
				.await;

			let after = self
				.after_side(user_id, pdu, snapshot, &before)
				.await;

			either_side(before, after)
		},
	};

	self.or_sticky(user_id, pdu, visible)
		.unwrap_or_else(|_| false)
		.await
}

/// Decides whether a user may see an event, keeping a failed read apart from a
/// denial.
///
/// Grants and denials follow [`Self::user_can_see_event`]. A failed read of the
/// snapshot, its history-visibility event or a membership record is returned as
/// an error unless another check grants the event regardless, such as the
/// sticky grant. On a failed history-visibility read `user_can_see_event`
/// assumes `shared` instead, and on any other it denies the event unless such a
/// check grants it. Absent history visibility is still `shared`, an absent
/// member event still `leave`, and an event recorded without state in a room
/// without any is still denied.
#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "trace")]
pub async fn try_user_can_see_event<Pdu>(&self, user_id: &UserId, pdu: &Pdu) -> Result<bool>
where
	Pdu: Event,
{
	let visible = match self.event_snapshot(pdu).await {
		| Ok(None) => return Ok(false),
		| Err(error) => Err(error),
		| Ok(Some(snapshot)) => {
			let history_visibility = self
				.try_history_visibility(snapshot.shortstatehash)
				.await;

			let before = self
				.visible_at(user_id, pdu, snapshot.shortstatehash, history_visibility)
				.await;

			let after = self
				.after_side(user_id, pdu, snapshot, &before)
				.await;

			either_side(before, after)
		},
	};

	self.or_sticky(user_id, pdu, visible).await
}

/// Selects the snapshot an event's visibility is judged against.
///
/// That is the event's own, or the room's current state for an event recorded
/// without one, and `None` when the room has no state either. Only a missing
/// snapshot falls back, so a failed read cannot widen access.
#[implement(super::Service)]
async fn event_snapshot<Pdu>(&self, pdu: &Pdu) -> Result<Option<Snapshot>>
where
	Pdu: Event,
{
	let state = &self.services.state;
	let Some(shortstatehash) = state
		.pdu_shortstatehash(pdu.event_id())
		.await
		.present()?
	else {
		return state
			.get_room_shortstatehash(pdu.room_id())
			.map_ok(|shortstatehash| Snapshot { shortstatehash, recorded: false })
			.await
			.present();
	};

	Ok(Some(Snapshot { shortstatehash, recorded: true }))
}

/// Checks the value a change of history visibility sets, when the value before
/// it did not grant the user and the event's own recorded state judged it.
///
/// An event judged by the room's current state instead, being backfilled or
/// only an outlier, gets no grant from its own content. Returning the future
/// directly spares the entry points an async frame of their own, which the
/// debug-build worker stack cannot afford.
#[implement(super::Service)]
fn after_side<'a, Pdu>(
	&'a self,
	user_id: &'a UserId,
	pdu: &'a Pdu,
	snapshot: Snapshot,
	before: &Result<bool>,
) -> OptionFuture<impl Future<Output = Result<bool>> + Send + use<'a, Pdu>>
where
	Pdu: Event + 'a,
{
	matches!(before, Ok(true))
		.is_false()
		.and_is(snapshot.recorded)
		.and_then(|| set_history_visibility(pdu))
		.map_async(|after| self.visible_after(user_id, pdu, snapshot.shortstatehash, after))
}

/// Judges a change of history visibility under the value it sets.
///
/// Membership is the same on both sides of such an event, which changes none.
/// Only an event the timeline accepted is judged so: a soft-failed one also has
/// its own recorded state, but clients are not shown it.
#[implement(super::Service)]
// cold arm, kept off the debug-build worker stack every visibility caller shares
#[cfg_attr(debug_assertions, tuwunel_core::async_noinline)]
async fn visible_after<'a, Pdu>(
	&'a self,
	user_id: &'a UserId,
	pdu: &'a Pdu,
	shortstatehash: ShortStateHash,
	after: HistoryVisibility,
) -> Result<bool>
where
	Pdu: Event + 'a,
{
	let event_id = pdu.event_id();
	let position = self
		.services
		.timeline
		.pdu_position(event_id)
		.await?;

	if position.pdu_id(event_id)?.is_none() {
		return Ok(false);
	}

	self.visible_at(user_id, pdu, shortstatehash, Ok(after))
		.await
}

/// Judges an event under the history visibility of its snapshot.
///
/// Read errors are returned to the caller, which can still grant a sticky
/// event.
#[implement(super::Service)]
async fn visible_at<Pdu>(
	&self,
	user_id: &UserId,
	pdu: &Pdu,
	shortstatehash: ShortStateHash,
	history_visibility: Result<HistoryVisibility>,
) -> Result<bool>
where
	Pdu: Event,
{
	let membership = || self.try_user_membership(shortstatehash, user_id);
	let own = || pdu.membership_for(user_id);

	match history_visibility {
		| Err(error) => Err(error),
		| Ok(HistoryVisibility::WorldReadable) => Ok(true),

		// Allow the user's own invite or join, or a user at least invited at the event
		| Ok(HistoryVisibility::Invited)
			if matches!(own(), Some(MembershipState::Join | MembershipState::Invite)) =>
			Ok(true),

		| Ok(HistoryVisibility::Invited) =>
			membership()
				.map_ok(|membership| {
					matches!(membership, MembershipState::Join | MembershipState::Invite)
				})
				.await,

		// Allow the user's own join, or a user joined at the event
		| Ok(HistoryVisibility::Joined) if matches!(own(), Some(MembershipState::Join)) =>
			Ok(true),

		| Ok(HistoryVisibility::Joined) =>
			membership()
				.map_ok(|membership| matches!(membership, MembershipState::Join))
				.await,

		// An unrecognized value is treated as shared.
		| Ok(HistoryVisibility::Shared | _) =>
			self.user_shared_history(shortstatehash, pdu, user_id)
				.await,
	}
}

/// Grants a joined user's sticky event (MSC4354) whatever its history
/// visibility.
///
/// A failed read on either side is the error only when neither grants the
/// event.
#[implement(super::Service)]
async fn or_sticky<Pdu>(&self, user_id: &UserId, pdu: &Pdu, visible: Result<bool>) -> Result<bool>
where
	Pdu: Event,
{
	if matches!(visible, Ok(true)) {
		return Ok(true);
	}

	let sticky = self.user_sees_sticky(user_id, pdu).await;

	if matches!(sticky, Ok(true)) {
		return Ok(true);
	}

	visible.and(sticky)
}

/// Whether a user currently joined to the room may see a sticky event its
/// history visibility hides (MSC4354).
///
/// The event must be unexpired by its own sticky object, which a redaction
/// removes, and by its index row, which only an accepted event has. A failed
/// read is the error only when the other read does not deny the event.
#[implement(super::Service)]
#[tracing::instrument(level = "trace", skip_all)]
async fn user_sees_sticky<Pdu>(&self, user_id: &UserId, pdu: &Pdu) -> Result<bool>
where
	Pdu: Event,
{
	let now = now_millis();

	if pdu.sticky_until(now).is_none() {
		return Ok(false);
	}

	let indexed = self
		.services
		.sticky
		.try_indexed_unexpired(pdu.event_id(), now)
		.await;

	if matches!(indexed, Ok(false)) {
		return Ok(false);
	}

	let joined = self
		.services
		.state_cache
		.try_is_joined(user_id, pdu.room_id())
		.await;

	if matches!(joined, Ok(false)) {
		return Ok(false);
	}

	indexed.and(joined)
}

/// Whether a user may see an event under `shared` history visibility.
///
/// A current member sees the whole room, which the first check answers without
/// touching room state; a user joined at the event sees it too, and a former
/// member keeps events through the departure ending their last join. A failed
/// read is the error only when no check grants the event.
#[implement(super::Service)]
async fn user_shared_history<Pdu>(
	&self,
	shortstatehash: ShortStateHash,
	pdu: &Pdu,
	user_id: &UserId,
) -> Result<bool>
where
	Pdu: Event,
{
	let joined = self
		.services
		.state_cache
		.try_is_joined(user_id, pdu.room_id())
		.await;

	if matches!(joined, Ok(true)) {
		return Ok(true);
	}

	let joined_at_event = self
		.try_user_membership(shortstatehash, user_id)
		.map_ok(|membership| matches!(membership, MembershipState::Join))
		.await;

	if matches!(joined_at_event, Ok(true)) {
		return Ok(true);
	}

	let departed_after = self.departed_after(pdu, user_id).await;

	if matches!(departed_after, Ok(true)) {
		return Ok(true);
	}

	joined.and(joined_at_event).and(departed_after)
}

/// Whether the departure ending a former member's last join falls no earlier
/// than the event.
///
/// A user who never joined, or whose departure or the event's position is
/// missing, is denied; a failed read or a position that does not decode is an
/// error.
#[implement(super::Service)]
async fn departed_after<Pdu>(&self, pdu: &Pdu, user_id: &UserId) -> Result<bool>
where
	Pdu: Event,
{
	let Some(departure) = self
		.retained_departure_count(user_id, pdu.room_id())
		.await?
	else {
		return Ok(false);
	};

	let event_id = pdu.event_id();
	let pdu_id = self
		.services
		.timeline
		.pdu_position(event_id)
		.await?
		.pdu_id(event_id)?;

	let visible = pdu_id.is_some_and(|pdu_id| pdu_id.pdu_count() <= departure);

	Ok(visible)
}

/// Reports whether a user may read the room's state events.
///
/// Whoever may read the current state (see `user_can_see_current_state`) is
/// admitted, as is a departed member, who reads their departure snapshot.
/// `user_visible_shortstatehash` makes the same decision when selecting a snapshot.
#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "trace")]
pub async fn user_can_see_state_events(&self, user_id: &UserId, room_id: &RoomId) -> bool {
	self.user_can_see_current_state(user_id, room_id)
		.await
		|| self.is_departed_member(user_id, room_id).await
}

/// Reports whether a user may read the room's current state.
///
/// A joined user may read it, as may anyone under world-readable history and a
/// pending invitee under invited history. Under `joined`, `shared` and any
/// value not understood (treated as `shared` by the spec), nobody else may.
#[implement(super::Service)]
async fn user_can_see_current_state(&self, user_id: &UserId, room_id: &RoomId) -> bool {
	if self
		.services
		.state_cache
		.is_joined(user_id, room_id)
		.await
	{
		return true;
	}

	match self.get_history_visibility(room_id).await {
		| HistoryVisibility::WorldReadable => true,
		| HistoryVisibility::Invited =>
			self.services
				.state_cache
				.is_invited(user_id, room_id)
				.await,
		| _ => false,
	}
}

/// Reports whether a user once joined a room and keeps a departure count there.
///
/// This is the count `departure_count` selects the snapshot by. A leave or ban
/// that ended only an invite or a knock, or banned a user who never joined,
/// marks no departed member, and forgetting the room drops the count.
#[implement(super::Service)]
async fn is_departed_member(&self, user_id: &UserId, room_id: &RoomId) -> bool {
	let state_cache = &self.services.state_cache;

	state_cache
		.get_left_count(room_id, user_id)
		.is_ok()
		.and(state_cache.once_joined(user_id, room_id))
		.await
}

/// Selects the state snapshot a user may read.
///
/// A departed member reads the state after their departure, or after an earlier
/// `at`; a user allowed the current state reads it, or the state after `at`.
/// Anyone else, including a member whose departure was forgotten, reads none.
/// Callers check access first; selection rechecks it so lapsed admission cannot widen reads.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn user_visible_shortstatehash(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
	at: Option<PduCount>,
) -> Result<Option<ShortStateHash>> {
	let departure = self.departure_count(user_id, room_id).await?;

	match (departure, at) {
		| (Some(departure), _) if at.is_none_or(|at| departure <= at) =>
			self.departure_shortstatehash(room_id, departure)
				.map_ok(Some)
				.await,

		| (None, _)
			if !self
				.user_can_see_current_state(user_id, room_id)
				.await =>
			Ok(None),

		| (_, Some(at)) =>
			self.services
				.timeline
				.shortstatehash_after(room_id, at)
				.map_ok(Some)
				.await,

		| (_, None) =>
			self.services
				.state
				.get_room_shortstatehash(room_id)
				.map_ok(Some)
				.map_err(|e| err!(Database("Missing state for {room_id:?}: {e:?}")))
				.await,
	}
}

/// Returns the timeline count at which a departed member left a room.
///
/// A user who once joined and keeps a leave or ban count is a departed member.
/// The retained row is an upper bound, not necessarily the event ending the
/// last join. If that event cannot be established, do not grant history. A
/// joined user, one who never joined, or one whose count is gone because they
/// forgot the room or were invited back, has none.
#[implement(super::Service)]
pub async fn departure_count(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result<Option<PduCount>> {
	if self
		.services
		.state_cache
		.try_is_joined(user_id, room_id)
		.await?
	{
		return Ok(None);
	}

	self.retained_departure_count(user_id, room_id)
		.await
}

#[implement(super::Service)]
async fn retained_departure_count(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result<Option<PduCount>> {
	let state_cache = &self.services.state_cache;

	if state_cache
		.try_once_joined(user_id, room_id)
		.await?
		.is_false()
	{
		return Ok(None);
	}

	// Sequenced, so a user whose left count is gone skips the member event read.
	let Some(left_count) = state_cache
		.get_left_count(room_id, user_id)
		.map_ok(PduCount::from_unsigned)
		.await
		.present()?
	else {
		return Ok(None);
	};

	self.departure_event_count(user_id, room_id)
		.map_ok(|count| Some(count.min(left_count)))
		.await
}

/// Walk the user's member events, newest first, to the transition out of
/// `join`. An unreadable or missing predecessor cannot prove a departure.
#[implement(super::Service)]
async fn departure_event_count(&self, user_id: &UserId, room_id: &RoomId) -> Result<PduCount> {
	let kind = StateEventType::RoomMember;
	let key = user_id.as_str();
	let room_state = self
		.services
		.state
		.get_room_shortstatehash(room_id)
		.await?;
	let mut member = self
		.state_get_checked(room_state, &kind, key)
		.await?;

	for _ in 0..MEMBERSHIP_STEPS {
		let before = self
			.services
			.state
			.pdu_shortstatehash(member.event_id())
			.await?;
		let previous = self.state_get_checked(before, &kind, key).await?;
		if previous.membership_for(user_id) == Some(MembershipState::Join) {
			let content: RoomMemberEventContent = member.get_content()?;
			if !matches!(content.membership, MembershipState::Leave | MembershipState::Ban) {
				return Err!(Request(Forbidden("No departure after the last join.")));
			}

			let event_id = member.event_id();
			let pdu_id = self
				.services
				.timeline
				.pdu_position(event_id)
				.await?
				.pdu_id(event_id)?;

			return pdu_id
				.map(RawPduId::pdu_count)
				.ok_or_else(|| err!(Database("Missing timeline position for {event_id}.")));
		}

		member = previous;
	}

	Err!(Request(Forbidden("Too many membership changes since the last join.")))
}

#[implement(super::Service)]
async fn state_get_checked(
	&self,
	shortstatehash: ShortStateHash,
	event_type: &StateEventType,
	state_key: &str,
) -> Result<Pdu> {
	let event_id = self
		.state_get_id(shortstatehash, event_type, state_key)
		.await?;

	match self
		.services
		.timeline
		.get_checked(&event_id)
		.await?
	{
		| Mapped::Damaged => Err!(Database("The timeline position of {event_id} is damaged.")),
		| Mapped::Event(event) => Ok(event),
	}
}

/// Returns the snapshot after a departed member's departure.
///
/// The departure is the member event at the count, and the snapshot after it
/// is the state at that event with the event applied, rather than the next
/// timeline event's snapshot: a remote event's snapshot follows its position
/// in the event graph, not in this timeline, so it can miss the departure or
/// include later state. A count holding no event, such as a departure
/// recorded by a state reset, an event without a snapshot, or a read-only
/// database, which cannot store the derived snapshot, falls back to the
/// snapshot preceding the next timeline event.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn departure_shortstatehash(
	&self,
	room_id: &RoomId,
	departure: PduCount,
) -> Result<ShortStateHash> {
	let after_event = self
		.services
		.db
		.is_read_only()
		.is_false()
		.then_async(|| {
			self.services
				.timeline
				.get_pdu_at(room_id, departure)
				.and_then(async |pdu| {
					self.services
						.state
						.shortstatehash_after_event(&pdu)
						.await
				})
		})
		.await
		.transpose()
		.optional()?
		.flatten();

	let Some(shortstatehash) = after_event else {
		return self
			.services
			.timeline
			.shortstatehash_after(room_id, departure)
			.await;
	};

	Ok(shortstatehash)
}

/// Reports whether a user may discover or inspect a room.
///
/// Current join, invite, retained left membership, or world-readable history
/// grants access. Forgetting a room clears the retained left membership and can
/// therefore remove this visibility.
#[implement(super::Service)]
pub async fn user_can_see_room(&self, user_id: &UserId, room_id: &RoomId) -> bool {
	let state_cache = &self.services.state_cache;
	let joined = state_cache.is_joined(user_id, room_id);
	let invited = state_cache.is_invited(user_id, room_id);
	let left = state_cache.is_left(user_id, room_id);
	let world_readable = self.is_world_readable(room_id);

	joined.or3(invited, left, world_readable).await
}

/// Reports whether a user may peek into a room, as a room preview does.
///
/// A member always may, and anyone else only while the room's history is
/// world-readable. Unlike `user_can_see_room`, a pending invite or a retained
/// left membership grants nothing here.
#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "trace")]
pub async fn user_can_peek(&self, user_id: &UserId, room_id: &RoomId) -> bool {
	// Sequenced rather than raced, so a member's point get skips the state read.
	self.services
		.state_cache
		.is_joined(user_id, room_id)
		.await
		|| self.is_world_readable(room_id).await
}

/// Reports whether the room's history was world-readable at an event.
///
/// A peek may show only such events, so an event without recorded state, or
/// with missing or invalid history visibility, does not qualify. The event that
/// makes the room world-readable counts as well, as the spec requires.
#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "trace")]
pub async fn is_world_readable_at<Pdu>(&self, pdu: &Pdu) -> bool
where
	Pdu: Event,
{
	let opens_history = pdu.is_type_and_state_key(&TimelineEventType::RoomHistoryVisibility, "")
		&& pdu
			.get_content()
			.is_ok_and(|c: RoomHistoryVisibilityEventContent| {
				c.history_visibility == HistoryVisibility::WorldReadable
			});

	opens_history
		|| self.history_visibility_at(pdu.event_id()).await
			== Some(HistoryVisibility::WorldReadable)
}

/// Returns the history visibility in the state an event was sent in.
///
/// Missing or invalid history visibility reads as `shared`, and `None` means
/// the event's state is unrecorded or unreadable.
#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "trace")]
async fn history_visibility_at(&self, event_id: &EventId) -> Option<HistoryVisibility> {
	self.services
		.state
		.pdu_shortstatehash(event_id)
		.await
		.ok()
		.map_async(|shortstatehash| self.history_visibility(shortstatehash))
		.await
}

/// Probes whether a sender may invite a target user.
///
/// The normal event-build, authorization, and signing path runs under the
/// caller's room-state lock, but the synthetic membership event is not stored.
/// Any build error denies the invite, while build-time ID allocations may remain.
#[implement(super::Service)]
pub async fn user_can_invite(
	&self,
	room_id: &RoomId,
	sender: &UserId,
	target_user: &UserId,
	state_lock: &RoomMutexGuard,
) -> bool {
	self.services
		.timeline
		.create_hash_and_sign_event(
			PduBuilder::state(
				target_user.as_str(),
				&RoomMemberEventContent::new(MembershipState::Invite),
			),
			sender,
			room_id,
			state_lock,
		)
		.await
		.is_ok()
}

/// Probes whether a user may send a room tombstone.
///
/// The user must currently be joined. The normal event-build, authorization,
/// and signing path evaluates a synthetic tombstone without storing it; any
/// build error denies permission, while build-time ID allocations may remain.
#[implement(super::Service)]
pub async fn user_can_tombstone(
	&self,
	room_id: &RoomId,
	user_id: &UserId,
	state_lock: &RoomMutexGuard,
) -> bool {
	if !self
		.services
		.state_cache
		.is_joined(user_id, room_id)
		.await
	{
		return false;
	}

	self.services
		.timeline
		.create_hash_and_sign_event(
			PduBuilder::state(StateKey::new(), &RoomTombstoneEventContent {
				replacement_room: room_id.into(), // placeholder,
				body: "Not a valid m.room.tombstone.".into(),
			}),
			user_id,
			room_id,
			state_lock,
		)
		.await
		.is_ok()
}

/// The value a change of history visibility sets.
///
/// Content that does not parse sets `shared`, as the snapshot readers read it.
fn set_history_visibility<Pdu>(pdu: &Pdu) -> Option<HistoryVisibility>
where
	Pdu: Event,
{
	pdu.is_type_and_state_key(&TimelineEventType::RoomHistoryVisibility, "")
		.then(|| {
			pdu.get_content().map_or(
				HistoryVisibility::Shared,
				|content: RoomHistoryVisibilityEventContent| content.history_visibility,
			)
		})
}

/// Combines the judgments before and after a change of history visibility.
///
/// A grant on either side outweighs a failed read on the other.
fn either_side(before: Result<bool>, after: Option<Result<bool>>) -> Result<bool> {
	match after {
		| None | Some(Ok(false)) => before,
		| Some(Ok(true)) => Ok(true),
		| Some(Err(error)) => before.and(Err(error)),
	}
}

#[cfg(test)]
mod tests;
