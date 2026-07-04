use ruma::{EventId, RoomId, UserId};
use tuwunel_core::{Err, Event, Result, warn};
use tuwunel_service::Services;

pub(crate) async fn invite_check(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
) -> Result {
	if services.config.block_non_admin_invites && !services.admin.user_is_admin(sender_user).await
	{
		warn!("{sender_user} is not an admin and attempted to send an invite to {room_id}");
		return Err!(Request(Forbidden("Invites are not allowed on this server.")));
	}

	Ok(())
}

pub(crate) async fn is_self_redaction(
	services: &Services,
	user_id: &UserId,
	event_id: &EventId,
) -> bool {
	services
		.timeline
		.get_pdu(event_id)
		.await
		.is_ok_and(|target| target.sender() == user_id)
}
