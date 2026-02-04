use ruma::{RoomId, UserId};
use tuwunel_core::{Err, Result, warn};
use tuwunel_service::Services;

pub(crate) async fn invite_check(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
) -> Result {
	if !services.admin.user_is_admin(sender_user).await && services.config.block_non_admin_invites
	{
		warn!("{sender_user} is not an admin and attempted to send an invite to {room_id}");
		return Err!(Request(Forbidden("Invites are not allowed on this server.")));
	}

	Ok(())
}
