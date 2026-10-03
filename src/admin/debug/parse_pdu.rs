use ruma::{OwnedEventId, RoomVersionId};
use tuwunel_core::{Err, Result, err, utils::string::EMPTY};
use tuwunel_matrix::pdu::PduEvent;

use crate::admin_command;

#[admin_command]
pub(super) async fn parse_pdu(&self) -> Result {
	if self.body.len() < 2
		|| !self.body[0].trim().starts_with("```")
		|| self.body.last().unwrap_or(&EMPTY).trim() != "```"
	{
		return Err!("Expected code block in command body. Add --help for details.");
	}

	let string = self.body[1..self.body.len().saturating_sub(1)].join("\n");
	let rules = RoomVersionId::V6
		.rules()
		.expect("rules for V6 rooms");

	let value =
		serde_json::from_str(&string).map_err(|e| err!("Invalid json in command body: {e}"))?;

	let hash = ruma::signatures::reference_hash(&value, &rules)
		.map_err(|e| err!("Could not parse PDU JSON: {e:?}"))?;

	let event_id = OwnedEventId::parse(format!("${hash}"));

	let value = serde_json::to_value(value)?;

	match serde_json::from_value::<PduEvent>(value) {
		| Err(e) => return Err!("EventId: {event_id:?}\nCould not parse event: {e}"),
		| Ok(pdu) => write!(self, "EventId: {event_id:?}\n{pdu:#?}"),
	}
	.await
}
