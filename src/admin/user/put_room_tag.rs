use ruma::{OwnedRoomId, events::tag::TagInfo};
use tuwunel_core::Result;

use crate::{admin_command, utils::parse_active_local_user_id};

#[admin_command]
pub(super) async fn put_room_tag(
	&self,
	user_id: String,
	room_id: OwnedRoomId,
	tag: String,
) -> Result {
	let user_id = parse_active_local_user_id(self.services, &user_id).await?;

	self.services
		.account_data
		.set_room_tag(&user_id, &room_id, tag.as_str().into(), Some(TagInfo::new()))
		.await?;

	write!(
		self,
		"Successfully updated room account data for {user_id} and room {room_id} with tag {tag}"
	)
	.await
}
