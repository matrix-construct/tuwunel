//! Evaluates federation visibility against historical room state.
//!
//! History visibility determines whether an origin may receive an event. A
//! separate strict membership check supports sender-erasure pruning.

use futures::StreamExt;
use ruma::{
	EventId, RoomId, ServerName, UserId,
	events::{
		StateEventType,
		room::history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
	},
};
use tuwunel_core::{implement, utils::stream::ReadyExt};

/// Reports whether a server may see an event over federation.
///
/// Missing event state is allowed, and missing or invalid history visibility
/// defaults to `shared`. For `invited` and `joined`, a currently joined user
/// from the origin must also hold the required membership at the event.
#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "trace")]
pub async fn server_can_see_event(
	&self,
	origin: &ServerName,
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

	let current_server_members = self
		.services
		.state_cache
		.room_members(room_id)
		.ready_filter(|member| member.server_name() == origin);

	match history_visibility {
		| HistoryVisibility::Invited => {
			// Allow if any member on requesting server was AT LEAST invited, else deny
			current_server_members
				.any(|member| self.user_was_invited(shortstatehash, member))
				.await
		},
		| HistoryVisibility::Joined => {
			// Allow if any member on requested server was joined, else deny
			current_server_members
				.any(|member| self.user_was_joined(shortstatehash, member))
				.await
		},
		| HistoryVisibility::WorldReadable | HistoryVisibility::Shared | _ => true,
	}
}

/// Reports whether any user from an origin was joined at an event.
///
/// This MSC4025 helper scans membership state at the event and denies when the
/// snapshot cannot be resolved. Invalid user state keys are skipped.
#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "trace")]
pub async fn server_joined_at_pdu(&self, origin: &ServerName, event_id: &EventId) -> bool {
	let Ok(shortstatehash) = self
		.services
		.state
		.pdu_shortstatehash(event_id)
		.await
	else {
		return false;
	};

	self.state_keys(shortstatehash, &StateEventType::RoomMember)
		.ready_filter_map(|state_key| UserId::parse(state_key.as_str()).ok())
		.ready_filter(|user_id| user_id.server_name() == origin)
		.any(async |user_id| {
			self.user_was_joined(shortstatehash, &user_id)
				.await
		})
		.await
}
