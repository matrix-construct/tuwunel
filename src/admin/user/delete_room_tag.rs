use ruma::OwnedRoomId;
use tuwunel_core::Result;

use crate::{admin_command, utils::parse_active_local_user_id};

#[admin_command]
pub(super) async fn delete_room_tag(
	&self,
	user_id: String,
	room_id: OwnedRoomId,
	tag: String,
) -> Result {
	let user_id = parse_active_local_user_id(self.services, &user_id).await?;

	self.services
		.account_data
		.remove_room_tag(&user_id, &room_id, &tag.as_str().into())
		.await?;

	write!(
		self,
		"Successfully updated room account data for {user_id} and room {room_id}, deleting room \
		 tag {tag}"
	)
	.await
}
