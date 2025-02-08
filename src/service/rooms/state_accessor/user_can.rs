use conduwuit::{error, implement, pdu::PduBuilder, Err, Error, Result};
use ruma::{
	events::{
		room::{
			history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
			member::{MembershipState, RoomMemberEventContent},
			power_levels::{RoomPowerLevels, RoomPowerLevelsEventContent},
		},
		StateEventType, TimelineEventType,
	},
	EventId, RoomId, UserId,
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
		.is_ok_and(|pdu| pdu.kind == TimelineEventType::RoomCreate)
	{
		return Err!(Request(Forbidden("Redacting m.room.create is not safe, forbidding.")));
	}

	if redacting_event
		.as_ref()
		.is_ok_and(|pdu| pdu.kind == TimelineEventType::RoomServerAcl)
	{
		return Err!(Request(Forbidden(
			"Redacting m.room.server_acl will result in the room being inaccessible for \
			 everyone (empty allow key), forbidding."
		)));
	}

	if let Ok(pl_event_content) = self
		.room_state_get_content::<RoomPowerLevelsEventContent>(
			room_id,
			&StateEventType::RoomPowerLevels,
			"",
		)
		.await
	{
		let pl_event: RoomPowerLevels = pl_event_content.into();
		Ok(pl_event.user_can_redact_event_of_other(sender)
			|| pl_event.user_can_redact_own_event(sender)
				&& if let Ok(redacting_event) = redacting_event {
					if federation {
						redacting_event.sender.server_name() == sender.server_name()
					} else {
						redacting_event.sender == sender
					}
				} else {
					false
				})
	} else {
		// Falling back on m.room.create to judge power level
		if let Ok(room_create) = self
			.room_state_get(room_id, &StateEventType::RoomCreate, "")
			.await
		{
			Ok(room_create.sender == sender
				|| redacting_event
					.as_ref()
					.is_ok_and(|redacting_event| redacting_event.sender == sender))
		} else {
			Err(Error::bad_database(
				"No m.room.power_levels or m.room.create events in database for room",
			))
		}
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
	let Ok(shortstatehash) = self.pdu_shortstatehash(event_id).await else {
		return true;
	};

	if let Some(visibility) = self
		.user_visibility_cache
		.lock()
		.expect("locked")
		.get_mut(&(user_id.to_owned(), shortstatehash))
	{
		return *visibility;
	}

	let currently_member = self.services.state_cache.is_joined(user_id, room_id).await;

	let history_visibility = self
		.state_get_content(shortstatehash, &StateEventType::RoomHistoryVisibility, "")
		.await
		.map_or(HistoryVisibility::Shared, |c: RoomHistoryVisibilityEventContent| {
			c.history_visibility
		});

	let visibility = match history_visibility {
		| HistoryVisibility::WorldReadable => true,
		| HistoryVisibility::Shared => currently_member,
		| HistoryVisibility::Invited => {
			// Allow if any member on requesting server was AT LEAST invited, else deny
			self.user_was_invited(shortstatehash, user_id).await
		},
		| HistoryVisibility::Joined => {
			// Allow if any member on requested server was joined, else deny
			self.user_was_joined(shortstatehash, user_id).await
		},
		| _ => {
			error!("Unknown history visibility {history_visibility}");
			false
		},
	};

	self.user_visibility_cache
		.lock()
		.expect("locked")
		.insert((user_id.to_owned(), shortstatehash), visibility);

	visibility
}

/// Whether a user is allowed to see an event, based on
/// the room's history_visibility at that event's state.
#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "trace")]
pub async fn user_can_see_state_events(&self, user_id: &UserId, room_id: &RoomId) -> bool {
	if self.services.state_cache.is_joined(user_id, room_id).await {
		return true;
	}

	let history_visibility = self
		.room_state_get_content(room_id, &StateEventType::RoomHistoryVisibility, "")
		.await
		.map_or(HistoryVisibility::Shared, |c: RoomHistoryVisibilityEventContent| {
			c.history_visibility
		});

	match history_visibility {
		| HistoryVisibility::Invited =>
			self.services.state_cache.is_invited(user_id, room_id).await,
		| HistoryVisibility::WorldReadable => true,
		| _ => false,
	}
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
