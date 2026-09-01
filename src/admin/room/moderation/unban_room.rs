use ruma::OwnedRoomOrAliasId;
use tuwunel_core::Result;

use crate::{admin_command, utils::room_enabled_reply};

#[admin_command]
pub(super) async fn unban_room(&self, room: OwnedRoomOrAliasId) -> Result {
	let room_id = self.services.alias.maybe_resolve(&room).await?;

	self.services.metadata.unban_room(&room_id);
	self.services.metadata.enable_room(&room_id);

	let message = room_enabled_reply(
		self.services,
		&room_id,
		"Room unbanned and federation re-enabled.",
		"Room unbanned",
	)
	.await;

	self.write_str(&message).await
}
