use ruma::OwnedRoomId;
use tuwunel_core::Result;

use crate::{admin_command, utils::room_enabled_reply};

#[admin_command]
pub(super) async fn enable_room(&self, room_id: OwnedRoomId) -> Result {
	self.services.metadata.enable_room(&room_id);

	let message =
		room_enabled_reply(self.services, &room_id, "Room enabled.", "Room enabled").await;

	self.write_str(&message).await
}
