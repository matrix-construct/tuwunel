use ruma::{OwnedRoomId, OwnedServerName};
use tuwunel_core::{Err, Result, err};

use crate::admin_command;

#[admin_command]
pub(super) async fn disable_room(&self, room_id: OwnedRoomId) -> Result {
	self.services.metadata.disable_room(&room_id);
	self.write_str("Room disabled.").await
}

#[admin_command]
pub(super) async fn enable_room(&self, room_id: OwnedRoomId) -> Result {
	self.services.metadata.enable_room(&room_id);
	self.write_str("Room enabled.").await
}

#[admin_command]
pub(super) async fn incoming_federation(&self) -> Result {
	Err!("This command is temporarily disabled")
}

#[admin_command]
pub(super) async fn fetch_support_well_known(&self, server_name: OwnedServerName) -> Result {
	let response = self
		.services
		.client
		.default
		.get(format!("https://{server_name}/.well-known/matrix/support"))
		.send()
		.await?;

	let text = response.text().await?;

	if text.is_empty() {
		return Err!("Response text/body is empty.");
	}

	if text.len() > 1500 {
		return Err!(
			"Response text/body is over 1500 characters, assuming no support well-known.",
		);
	}

	let pretty_json = serde_json::from_str(&text)
		.and_then(|json: serde_json::Value| serde_json::to_string_pretty(&json))
		.map_err(|_| err!("Response text/body is not valid JSON."))?;

	self.write_str(&format!("Got JSON response:\n\n```json\n{pretty_json}\n```"))
		.await
}
