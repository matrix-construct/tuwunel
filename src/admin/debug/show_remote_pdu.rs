use ruma::{OwnedEventId, OwnedServerName, api::federation::event::get_event};
use serde_json::Value;
use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
pub(super) async fn show_remote_pdu(
	&self,
	event_id: OwnedEventId,
	server: OwnedServerName,
) -> Result {
	let request = get_event::v1::Request { event_id };

	let response = self
		.services
		.federation
		.execute(&server, request)
		.await?;

	let json = response.pdu.get();

	let value = serde_json::from_str::<Value>(json)?;
	let pretty_json = serde_json::to_string_pretty(&value)?;

	write!(self, "```json\n{pretty_json}\n```").await
}
