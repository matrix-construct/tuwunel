use futures::StreamExt;
use ruma::{
	EventId, RoomId, ServerName, UserId,
	events::{
		StateEventType,
		room::history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
	},
};
use tuwunel_core::{implement, utils::stream::ReadyExt};

/// Whether a server is allowed to see an event through federation, based on
/// the room's history_visibility at that event's state.
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

/// MSC4025: whether any of `origin`'s users were joined in the room state at
/// the given event. Unresolvable state denies, matching the reference
/// implementation's erasure rule.
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
