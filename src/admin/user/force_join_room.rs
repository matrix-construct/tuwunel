use ruma::OwnedRoomOrAliasId;
use tuwunel_core::Result;
use tuwunel_service::membership::Join;

use crate::{admin_command, utils::parse_local_user_id};

#[admin_command]
pub(super) async fn force_join_room(&self, user_id: String, room: OwnedRoomOrAliasId) -> Result {
	let user_id = parse_local_user_id(self.services, &user_id)?;
	let (room_id, servers) = self
		.services
		.alias
		.maybe_resolve_with_servers(&room, None)
		.await?;

	assert!(
		self.services.globals.user_is_local(&user_id),
		"Parsed user_id must be a local user"
	);

	self.services
		.membership
		.join(Join {
			sender_user: &user_id,
			room_id: &room_id,
			orig_room_id: Some(&room),
			reason: None,
			servers: &servers,
			is_appservice: false,
			extra_content: None,
		})
		.await?;

	write!(self, "{user_id} has been joined to {room_id}.").await
}
