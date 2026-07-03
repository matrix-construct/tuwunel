use ruma::{
	RoomId, UserId,
	events::room::member::{MembershipState, RoomMemberEventContent},
};
use tuwunel_core::{Result, implement, pdu::PduBuilder};

use super::Service;
use crate::rooms::timeline::RoomMutexGuard;

#[implement(Service)]
#[tracing::instrument(
    level = "debug",
    skip_all,
    fields(%sender_user, %room_id, %user_id)
)]
pub async fn ban(
	&self,
	room_id: &RoomId,
	user_id: &UserId,
	reason: Option<&String>,
	sender_user: &UserId,
	state_lock: &RoomMutexGuard,
) -> Result {
	self.services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(user_id.to_string(), &RoomMemberEventContent {
				membership: MembershipState::Ban,
				reason: reason.cloned(),
				displayname: None,
				avatar_url: None,
				blurhash: None,
				is_direct: false,
				join_authorized_via_users_server: None,
				third_party_invite: None,
			}),
			sender_user,
			room_id,
			state_lock,
		)
		.await?;

	Ok(())
}
