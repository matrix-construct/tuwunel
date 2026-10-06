use ruma::{OwnedEventId, events::room::redaction::RoomRedactionEventContent};
use tuwunel_core::{
	Err, Result,
	matrix::{Event, PduEvent, pdu::PduBuilder},
};
use tuwunel_service::Services;

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

	let redaction_event_id = redact(self.services, &event).await?;

	write!(self, "Successfully redacted event. Redaction event ID: {redaction_event_id}").await
}

pub(super) async fn redact(services: &Services, event: &PduEvent) -> Result<OwnedEventId> {
	let reason = format!(
		"The administrator(s) of {} has redacted this user's message.",
		services.globals.server_name()
	);

	let state_lock = services.state.mutex.lock(event.room_id()).await;
	let event_id = || event.event_id().to_owned();
	let builder = PduBuilder {
		redacts: Some(event_id()),
		..PduBuilder::timeline(&RoomRedactionEventContent {
			redacts: Some(event_id()),
			reason: Some(reason),
		})
	};

	services
		.timeline
		.build_and_append_pdu(builder, event.sender(), event.room_id(), &state_lock)
		.await
}
