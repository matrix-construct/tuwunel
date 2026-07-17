use ruma::OwnedRoomOrAliasId;
use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
pub(super) async fn directory_unpublish(&self, room: OwnedRoomOrAliasId) -> Result {
	let room_id = self.services.alias.maybe_resolve(&room).await?;

	self.services.directory.set_not_public(&room_id);

	self.write_str("Room unpublished").await
}
