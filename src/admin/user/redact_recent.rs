use std::num::NonZeroUsize;

use futures::{StreamExt, TryStreamExt};
use ruma::{OwnedRoomOrAliasId, events::TimelineEventType};
use tuwunel_core::{Result, err, matrix::Event, utils::stream::TryReadyExt};

use super::redact_event::redact;
use crate::{admin_command, utils::parse_existing_local_user_id};

#[admin_command]
pub(super) async fn redact_recent(
	&self,
	user_id: String,
	room_id: OwnedRoomOrAliasId,
	count: NonZeroUsize,
) -> Result {
	let user_id = parse_existing_local_user_id(self.services, &user_id).await?;
	let room_id = self
		.services
		.alias
		.maybe_resolve(&room_id)
		.await?;

	let redacted = self
		.services
		.timeline
		.pdus_rev(None, &room_id, None)
		.ready_and_then(|item| {
			self.services
				.server
				.check_running()
				.map(|()| item)
		})
		.ready_try_filter(|(_, event)| {
			event.sender() == user_id
				&& event.state_key().is_none()
				&& !event.is_redacted()
				&& matches!(
					event.kind(),
					TimelineEventType::RoomMessage | TimelineEventType::RoomEncrypted
				)
		})
		.take(count.get())
		.try_fold(0_usize, async |redacted, (_, event)| {
			redact(self.services, &event)
				.await
				.map_err(|error| {
					err!(
						"Stopped after {redacted} successful redactions at {}: {error}. The \
						 failing event may already have been redacted.",
						event.event_id()
					)
				})?;

			Ok(redacted.saturating_add(1))
		})
		.await?;

	write!(
		self,
		"Redacted {redacted} of up to {count} messages from {user_id} in {room_id}."
	)
	.await
}
