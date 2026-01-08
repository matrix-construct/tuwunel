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
	Err, Result, implement,
	matrix::{Event, StateKey},
	pdu::PduBuilder,
};

use crate::rooms::state::RoomMutexGuard;

/// Checks if a given user can redact a given event
///
/// If federation is true, it allows redaction events from any user of the
/// same server as the original event sender
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

/// Whether a user is allowed to see an event, based on
/// the room's history_visibility at that event's state.
#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "trace")]
pub async fn user_can_see_event(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
	event_id: &EventId,
) -> bool {
	let Ok(shortstatehash) = self
		.services
		.state
		.pdu_shortstatehash(event_id)
		.await
	else {
		return true;
	};

	let history_visibility = self
		.state_get_content(shortstatehash, &StateEventType::RoomHistoryVisibility, "")
		.await
		.map_or(HistoryVisibility::Shared, |c: RoomHistoryVisibilityEventContent| {
			c.history_visibility
		});

	match history_visibility {
		| HistoryVisibility::WorldReadable => true,

		// Allow if any member on requesting server was AT LEAST invited, else deny
		| HistoryVisibility::Invited =>
			self.user_was_invited(shortstatehash, user_id)
				.await,

		// Allow if any member on requested server was joined, else deny
		| HistoryVisibility::Joined =>
			self.user_was_joined(shortstatehash, user_id)
				.await,

		| HistoryVisibility::Shared | _ =>
			self.services
				.state_cache
				.is_joined(user_id, room_id)
				.await,
	}
}

/// Whether a user is allowed to see an event, based on
/// the room's history_visibility at that event's state.
#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "trace")]
pub async fn user_can_access_room(&self, user_id: &UserId, room_id: &RoomId) -> bool {
	if self
		.services
		.state_cache
		.is_joined(user_id, room_id)
		.await
	{
		return true;
	}

	let history_visibility = self
		.room_state_get_content::<RoomHistoryVisibilityEventContent>(
			room_id,
			&StateEventType::RoomHistoryVisibility,
			"",
		)
		.await
		.ok()
		.map(|content| content.history_visibility);

	return matches!(history_visibility, Some(HistoryVisibility::WorldReadable));
}

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

#[implement(super::Service)]
pub async fn get_last_accessible_state_for_user(
	&self,
	room_id: &RoomId,
	user_id: &UserId,
) -> Result<u64> {
	if !self
		.services
		.state_cache
		.once_joined(user_id, room_id)
		.await
	{
		return Err!(Request(NotFound(debug_warn!(
			"You don't have permission to view the room state."
		))));
	}

	let room_shortstatehash = self
		.services
		.state
		.get_room_shortstatehash(room_id)
		.await?;

	let shortstatehash = if self
		.services
		.state_cache
		.is_joined(user_id, room_id)
		.await
	{
		room_shortstatehash
	} else {
		let pdu = self
			.services
			.state_accessor
			.state_get(room_shortstatehash, &StateEventType::RoomMember, user_id.as_str())
			.await
			.expect("membership event to be present if user was once joined");

		let event_id = pdu.event_id();

		debug_assert!(
			!matches!(
				pdu.get_content::<RoomMemberEventContent>()?
					.membership,
				MembershipState::Join
			),
			"user is not joined but last pdu membership is Joined, event id {event_id}",
		);

		self.services
			.state
			.pdu_shortstatehash(event_id)
			.await?
	};

	Ok(shortstatehash)
}
