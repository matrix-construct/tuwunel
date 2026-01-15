use ruma::EventId;
use tuwunel_core::{
	Result, err, implement,
	matrix::event::Event,
	utils::{self},
};

use super::ExtractBody;
use crate::rooms::short::ShortRoomId;

/// Replace a PDU with the redacted form.
#[implement(super::Service)]
#[tracing::instrument(name = "redact", level = "debug", skip(self))]
pub async fn redact_pdu<Pdu: Event + Send + Sync>(
	&self,
	event_id: &EventId,
	reason: &Pdu,
	shortroomid: ShortRoomId,
) -> Result {
	// TODO: Don't reserialize, keep original json
	let Ok(pdu_id) = self.get_pdu_id(event_id).await else {
		// If event does not exist, just noop
		return Ok(());
	};

	let mut pdu = self
		.get_pdu_from_id(&pdu_id)
		.await
		.map(Event::into_pdu)
		.map_err(|e| {
			err!(Database(error!(?pdu_id, ?event_id, ?e, "PDU ID points to invalid PDU.")))
		})?;

	if let Ok(content) = pdu.get_content::<ExtractBody>()
		&& let Some(body) = content.body
	{
		self.services
			.search
			.deindex_pdu(shortroomid, &pdu_id, &body);
	}

	let room_version_id = self
		.services
		.state
		.get_room_version(pdu.room_id())
		.await?;

	pdu.redact(&room_version_id, reason.to_value())?;

	let obj = utils::to_canonical_object(&pdu).map_err(|e| {
		err!(Database(error!(?event_id, ?e, "Failed to convert PDU to canonical JSON")))
	})?;

	self.replace_pdu(&pdu_id, &obj).await
}
