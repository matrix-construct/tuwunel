//! Evaluates user visibility and event-authoring permissions.
//!
//! Historical visibility checks combine event-time state with retained
//! membership metadata. Invite and tombstone checks use the normal event-build
//! pipeline as non-persisting authorization probes.

use futures::TryFutureExt;
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
	utils::{BoolExt, FutureBoolExt, OptionExt, future::TryExtExt, result::NotFound},
};
use tuwunel_matrix::{Event, PduCount, StateKey, pdu::PduBuilder};

use crate::rooms::{short::ShortStateHash, state::RoomMutexGuard};

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
/// event. Under `joined` and `invited`, the user's own membership event is
/// visible when the membership it sets qualifies, per the spec's
/// before-or-after rule.
#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "trace")]
pub async fn user_can_see_event<Pdu>(&self, user_id: &UserId, pdu: &Pdu) -> bool
where
	Pdu: Event,
{
	let room_id = pdu.room_id();
	let Ok(shortstatehash) = self
		.services
		.state
		.pdu_shortstatehash(pdu.event_id())
		.or_else(async |error| {
			// Only a missing snapshot falls back, so a failed read cannot widen access.
			if !error.is_not_found() {
				return Err(error);
			}

			self.services
				.state
				.get_room_shortstatehash(room_id)
				.await
		})
		.await
	else {
		return false;
	};

	match self.history_visibility(shortstatehash).await {
		| HistoryVisibility::WorldReadable => true,

		// Allow the user's own invite or join, or a user at least invited at the event
		| HistoryVisibility::Invited =>
			matches!(
				pdu.membership_for(user_id),
				Some(MembershipState::Join | MembershipState::Invite)
			) || self
				.user_was_invited(shortstatehash, user_id)
				.await,

		// Allow the user's own join, or a user joined at the event
		| HistoryVisibility::Joined =>
			matches!(pdu.membership_for(user_id), Some(MembershipState::Join))
				|| self
					.user_was_joined(shortstatehash, user_id)
					.await,

		// An unrecognized value is treated as shared.
		| HistoryVisibility::Shared | _ =>
			self.user_shared_history(shortstatehash, room_id, pdu.event_id(), user_id)
				.await,
	}
}

/// Whether a user may see an event under `shared` history visibility.
///
/// A current member sees the whole room, which the first check answers without
/// touching room state. A former member keeps events through their latest
/// leave, and lookup failures deny access.
#[implement(super::Service)]
async fn user_shared_history(
	&self,
	shortstatehash: ShortStateHash,
	room_id: &RoomId,
	event_id: &EventId,
	user_id: &UserId,
) -> bool {
	let state_cache = &self.services.state_cache;

	if state_cache.is_joined(user_id, room_id).await
		|| self
			.user_was_joined(shortstatehash, user_id)
			.await
	{
		return true;
	}

	if !state_cache.once_joined(user_id, room_id).await {
		return false;
	}

	let Ok(left_count) = state_cache.get_left_count(room_id, user_id).await else {
		return false;
	};

	let Ok(event_count) = self
		.services
		.timeline
		.get_pdu_count(event_id)
		.await
	else {
		return false;
	};

	event_count <= PduCount::from_unsigned(left_count)
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
/// The count is bounded by the leave or ban in current state, so one
/// overwritten by a later count never reaches past the real departure. A
/// joined user, one who never joined, or one whose count is gone because they
/// forgot the room or were invited back, has none.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn departure_count(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result<Option<PduCount>> {
	let state_cache = &self.services.state_cache;

	if state_cache.is_joined(user_id, room_id).await
		|| !state_cache.once_joined(user_id, room_id).await
	{
		return Ok(None);
	}

	// Sequenced, so a user whose left count is gone skips the member event read.
	let Some(left_count) = state_cache
		.get_left_count(room_id, user_id)
		.map_ok(PduCount::from_unsigned)
		.await
		.optional()?
	else {
		return Ok(None);
	};

	self.departure_event_count(user_id, room_id)
		.map_ok(|event_count| event_count.into_iter().chain([left_count]).min())
		.await
}

/// Returns the timeline count of the user's leave or ban in current state.
///
/// There is none when the member event is absent or sets any other
/// membership, or when the event has no timeline count.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
async fn departure_event_count(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result<Option<PduCount>> {
	let Some(member) = self
		.room_state_get(room_id, &StateEventType::RoomMember, user_id.as_str())
		.await
		.optional()?
	else {
		return Ok(None);
	};

	let content: RoomMemberEventContent = member.get_content()?;

	if !matches!(content.membership, MembershipState::Leave | MembershipState::Ban) {
		return Ok(None);
	}

	self.services
		.timeline
		.get_pdu_count(member.event_id())
		.await
		.optional()
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

#[cfg(test)]
mod tests;
