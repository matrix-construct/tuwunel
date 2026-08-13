use axum::extract::State;
use futures::{TryFutureExt, pin_mut};
use ruma::{api::client::membership::forget_room, events::room::member::MembershipState};
use tuwunel_core::{
	Err, Result, is_matching,
	utils::{FutureBoolExt, TryFutureExtExt},
};

use crate::Ruma;

/// # `POST /_matrix/client/v3/rooms/{roomId}/forget`
///
/// Forgets a room the sender user has left.
///
/// A user still joined, knocked, or invited must leave the room first. Any
/// other call succeeds, including a repeat and one for a room whose state the
/// server no longer holds.
///
/// The user's other devices have no way of knowing the room was forgotten, so
/// this has to be called from every device.
pub(crate) async fn forget_room_route(
	State(services): State<crate::State>,
	body: Ruma<forget_room::v3::Request>,
) -> Result<forget_room::v3::Response> {
	let user_id = body.sender_user();
	let room_id = &body.room_id;

	let joined = services.state_cache.is_joined(user_id, room_id);
	let knocked = services.state_cache.is_knocked(user_id, room_id);
	let invited = services.state_cache.is_invited(user_id, room_id);

	pin_mut!(joined, knocked, invited);
	if joined.or(knocked).or(invited).await {
		return Err!(Request(Unknown("You must leave the room before forgetting it")));
	}

	let left = services.state_cache.is_left(user_id, room_id);
	let left_or_banned = services
		.state_accessor
		.get_member(room_id, user_id)
		.map_ok(|member| member.membership)
		.map_ok_or(false, is_matching!(MembershipState::Leave | MembershipState::Ban));

	pin_mut!(left, left_or_banned);
	if left.or(left_or_banned).await {
		services.state_cache.forget(room_id, user_id);
	}

	Ok(forget_room::v3::Response::new())
}
