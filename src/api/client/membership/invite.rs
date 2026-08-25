use axum::extract::State;
use futures::{FutureExt, join};
use ruma::{
	api::client::membership::invite_user::{
		self,
		v3::{InvitationRecipient, InviteUserId},
	},
	events::room::member::MembershipState,
};
use tuwunel_core::{Err, Result};

use super::banned_room_check;
use crate::{ClientIp, Ruma, client::utils::invite_check};

/// # `POST /_matrix/client/r0/rooms/{roomId}/invite`
///
/// Tries to send an invite event into the room.
#[tracing::instrument(skip_all, fields(%client), name = "invite")]
pub(crate) async fn invite_user_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<invite_user::v3::Request>,
) -> Result<invite_user::v3::Response> {
	let sender_user = body.sender_user();

	let room_id = &body.room_id;

	invite_check(&services, sender_user, room_id).await?;

	banned_room_check(&services, sender_user, room_id, None, client).await?;

	let InvitationRecipient::UserId(InviteUserId { user_id, reason }) = &body.recipient else {
		return Err!(Request(ThreepidDenied("Third party identifiers are not implemented")));
	};

	// TODO: this should be in the service, but moving it from here would run the
	// sender's ignore-list check before the banned check, revealing the ignore
	// state to the sending user if the recipient is banned
	let member = services
		.state_accessor
		.get_member(room_id, user_id);

	let ignored = services
		.users
		.user_is_ignored(user_id, sender_user);

	let (member, ignored) = join!(member, ignored);

	if member.is_ok_and(|member| member.membership == MembershipState::Ban) {
		return Err!(Request(Forbidden("User is banned from this room.")));
	}

	if ignored {
		// silently drop the invite to the recipient if they've been ignored by the
		// sender, pretend it worked
		return Ok(invite_user::v3::Response {});
	}

	services
		.membership
		.invite(sender_user, user_id, room_id, reason.as_ref(), false)
		.boxed()
		.await?;

	Ok(invite_user::v3::Response {})
}
