use ruma::{OwnedEventId, events::room::redaction::RoomRedactionEventContent};
use tuwunel_core::{Err, Result};
use tuwunel_matrix::{Event, pdu::PduBuilder};

use crate::admin_command;

#[admin_command]
pub(super) async fn redact_event(&self, event_id: OwnedEventId) -> Result {
	let Ok(event) = self
		.services
		.timeline
		.get_non_outlier_pdu(&event_id)
		.await
	else {
		return Err!("Event does not exist in our database.");
	};

	if event.is_redacted() {
		return Err!("Event is already redacted.");
	}

	if !self
		.services
		.globals
		.user_is_local(event.sender())
	{
		return Err!("This command only works on local users.");
	}

	let reason = format!(
		"The administrator(s) of {} has redacted this user's message.",
		self.services.globals.server_name()
	);

	let redaction_event_id = {
		let state_lock = self
			.services
			.state
			.mutex
			.lock(event.room_id())
			.await;

		self.services
			.timeline
			.build_and_append_pdu(
				PduBuilder {
					redacts: Some(event.event_id().to_owned()),
					..PduBuilder::timeline(&RoomRedactionEventContent {
						redacts: Some(event.event_id().to_owned()),
						reason: Some(reason),
					})
				},
				event.sender(),
				event.room_id(),
				&state_lock,
			)
			.await?
	};

	write!(self, "Successfully redacted event. Redaction event ID: {redaction_event_id}").await
}
