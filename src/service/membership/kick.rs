use ruma::{
	RoomId, UserId,
	events::room::member::{MembershipState, RoomMemberEventContent},
};
use tuwunel_core::{Err, Result, implement, pdu::PduBuilder};

use super::Service;
use crate::rooms::timeline::RoomMutexGuard;

#[implement(Service)]
#[tracing::instrument(
    level = "debug",
    skip_all,
    fields(%sender_user, %room_id, %user_id)
)]
pub async fn kick(
	&self,
	room_id: &RoomId,
	user_id: &UserId,
	reason: Option<&String>,
	sender_user: &UserId,
	state_lock: &RoomMutexGuard,
) -> Result {
	let Ok(event) = self
		.services
		.state_accessor
		.get_member(room_id, user_id)
		.await
	else {
		return Err!(Request(Forbidden("Cannot kick a user who is not in the room.")));
	};

	// this is required to prevent ban -> leave transitions
	if !matches!(
		event.membership,
		MembershipState::Invite | MembershipState::Knock | MembershipState::Join,
	) {
		return Err!(Request(Forbidden(
			"Cannot kick a user who is not apart of the room (current membership: {})",
			event.membership
		)));
	}

	self.services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(user_id.to_string(), &RoomMemberEventContent {
				membership: MembershipState::Leave,
				reason: reason.cloned(),
				is_direct: false,
				join_authorized_via_users_server: None,
				third_party_invite: None,
				..event
			}),
			sender_user,
			room_id,
			state_lock,
		)
		.await?;

	Ok(())
}
