use ruma::{
	RoomId, UserId,
	events::room::member::{MembershipState, RoomMemberEventContent},
};
use tuwunel_core::{Err, Result, implement, pdu::PduBuilder};

use super::Service;
use crate::rooms::timeline::RoomMutexGuard;

#[implement(Service)]
#[tracing::instrument(
	name = "remote",
    level = "debug",
    skip_all,
    fields(%sender_user, %room_id, %user_id),
)]
pub async fn unban(
	&self,
	room_id: &RoomId,
	user_id: &UserId,
	reason: Option<&String>,
	sender_user: &UserId,
	state_lock: &RoomMutexGuard,
) -> Result {
	let current_member_content = self
		.services
		.state_accessor
		.get_member(room_id, user_id)
		.await
		.unwrap_or_else(|_| RoomMemberEventContent::new(MembershipState::Leave));

	if current_member_content.membership != MembershipState::Ban {
		return Err!(Request(Forbidden(
			"Cannot unban a user who is not banned (current membership: {})",
			current_member_content.membership
		)));
	}

	self.services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(user_id.to_string(), &RoomMemberEventContent {
				membership: MembershipState::Leave,
				reason: reason.cloned(),
				join_authorized_via_users_server: None,
				third_party_invite: None,
				is_direct: false,
				..current_member_content
			}),
			sender_user,
			room_id,
			state_lock,
		)
		.await?;

	Ok(())
}
