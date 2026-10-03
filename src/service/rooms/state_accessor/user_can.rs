//! Evaluates user visibility and event-authoring permissions.
//!
//! Historical visibility checks combine event-time state with retained
//! membership metadata. Invite and tombstone checks use the normal event-build
//! pipeline as non-persisting authorization probes.

use futures::pin_mut;
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
use tuwunel_core::{Err, Result, implement, utils::FutureBoolExt};
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
/// Missing event state is allowed, and missing or invalid history visibility
/// defaults to `shared`. The `shared` decision also accounts for the user's
/// membership intervals around the event. Under `joined` and `invited`, the
/// user's own membership event is visible when the membership it sets
/// qualifies, per the spec's before-or-after rule.
#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "trace")]
pub async fn user_can_see_event<Pdu>(&self, user_id: &UserId, pdu: &Pdu) -> bool
where
	Pdu: Event,
{
	let Some((shortstatehash, history_visibility)) =
		self.history_visibility_at(pdu.event_id()).await
	else {
		return true;
	};

	match history_visibility {
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
			self.user_shared_history(shortstatehash, pdu.room_id(), pdu.event_id(), user_id)
				.await,
	}
}

/// The room state an event was sent in and the history visibility it carried.
///
/// Missing or invalid history visibility reads as `shared`, and `None` means
/// the event has no recorded state.
#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "trace")]
async fn history_visibility_at(
	&self,
	event_id: &EventId,
) -> Option<(ShortStateHash, HistoryVisibility)> {
	let shortstatehash = self
		.services
		.state
		.pdu_shortstatehash(event_id)
		.await
		.ok()?;

	let history_visibility = self
		.state_get_content(shortstatehash, &StateEventType::RoomHistoryVisibility, "")
		.await
		.map_or(HistoryVisibility::Shared, |c: RoomHistoryVisibilityEventContent| {
			c.history_visibility
		});

	Some((shortstatehash, history_visibility))
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

/// Reports whether a user may read the room's current state events.
///
/// Current membership grants immediate access. Otherwise world-readable
/// history grants access; invited and shared visibility consult current
/// invitation or retained once-joined metadata. Missing visibility defaults
/// to `shared`.
#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "trace")]
pub async fn user_can_see_state_events(&self, user_id: &UserId, room_id: &RoomId) -> bool {
	if self
		.services
		.state_cache
		.is_joined(user_id, room_id)
		.await
	{
		return true;
	}

	let history_visibility = self
		.room_state_get_content(room_id, &StateEventType::RoomHistoryVisibility, "")
		.await
		.map_or(HistoryVisibility::Shared, |c: RoomHistoryVisibilityEventContent| {
			c.history_visibility
		});

	match history_visibility {
		| HistoryVisibility::WorldReadable => true,

		| HistoryVisibility::Invited =>
			self.services
				.state_cache
				.is_invited(user_id, room_id)
				.await,

		| HistoryVisibility::Shared =>
			self.services
				.state_cache
				.once_joined(user_id, room_id)
				.await,

		| _ => false,
	}
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

	pin_mut!(joined, invited, left, world_readable);
	joined
		.or(invited)
		.or(left)
		.or(world_readable)
		.await
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
		|| self
			.history_visibility_at(pdu.event_id())
			.await
			.is_some_and(|(_, history_visibility)| {
				history_visibility == HistoryVisibility::WorldReadable
			})
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
