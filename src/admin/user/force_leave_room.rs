use ruma::OwnedRoomOrAliasId;
use tuwunel_core::{Err, Result};

use crate::{admin_command, utils::parse_local_user_id};

#[admin_command]
pub(super) async fn force_leave_room(
	&self,
	user_id: String,
	room_id: OwnedRoomOrAliasId,
) -> Result {
	let user_id = parse_local_user_id(self.services, &user_id)?;
	let room_id = self
		.services
		.alias
		.maybe_resolve(&room_id)
		.await?;

	assert!(
		self.services.globals.user_is_local(&user_id),
		"Parsed user_id must be a local user"
	);

	if !self
		.services
		.state_cache
		.is_joined(&user_id, &room_id)
		.await
	{
		return Err!("{user_id} is not joined in the room");
	}

	let state_lock = self.services.state.mutex.lock(&room_id).await;

	self.services
		.membership
		.leave(&user_id, &room_id, None, false, &state_lock)
		.await?;

	drop(state_lock);

	write!(self, "{user_id} has left {room_id}.").await
}
