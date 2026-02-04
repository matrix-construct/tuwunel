use axum::extract::State;
use futures::{FutureExt, StreamExt, pin_mut};
use ruma::{
	api::client::membership::{
		get_member_events::{self},
		joined_members::{self, v3::RoomMember},
	},
	events::{
		StateEventType,
		room::{
			history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
			member::{MembershipState, RoomMemberEventContent},
		},
	},
};
use tuwunel_core::{
	Err, Result, at, is_equal_to, is_not_equal_to,
	matrix::Event,
	utils::{
		future::{BoolExt, TryExtExt},
		stream::ReadyExt,
	},
};

use crate::Ruma;

/// # `POST /_matrix/client/r0/rooms/{roomId}/members`
///
/// Lists all joined users in a room (TODO: at a specific point in time, with a
/// specific membership).
///
/// - Only works if the user is currently joined
pub(crate) async fn get_member_events_route(
	State(services): State<crate::State>,
	body: Ruma<get_member_events::v3::Request>,
) -> Result<get_member_events::v3::Response> {
	if !services
		.state_accessor
		.user_can_see_state_events(body.sender_user(), &body.room_id)
		.await
	{
		return Err!(Request(Forbidden(
			"You aren't a member of the room and weren't previously a member of the room."
		)));
	}

	let membership = body.membership.as_ref();
	let not_membership = body.not_membership.as_ref();
	Ok(get_member_events::v3::Response {
		chunk: services
			.state_accessor
			.room_state_full(&body.room_id)
			.ready_filter_map(Result::ok)
			.ready_filter(|((ty, _), _)| *ty == StateEventType::RoomMember)
			.map(at!(1))
			.ready_filter(|pdu| {
				pdu.get_content::<RoomMemberEventContent>()
					.is_ok_and(|content| {
						let event_membership = content.membership;

						membership.is_none_or(is_equal_to!(&event_membership))
							&& not_membership.is_none_or(is_not_equal_to!(&event_membership))
					})
			})
			.map(Event::into_format)
			.collect()
			.boxed()
			.await,
	})
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/joined_members`
///
/// Lists all members of a room.
///
/// - The sender user must be in the room
/// - TODO: An appservice just needs a puppet joined
pub(crate) async fn joined_members_route(
	State(services): State<crate::State>,
	body: Ruma<joined_members::v3::Request>,
) -> Result<joined_members::v3::Response> {
	let is_joined = services
		.state_cache
		.is_joined(body.sender_user(), &body.room_id);

	let is_world_readable = services
		.state_accessor
		.room_state_get_content(&body.room_id, &StateEventType::RoomHistoryVisibility, "")
		.map_ok_or(false, |c: RoomHistoryVisibilityEventContent| {
			c.history_visibility == HistoryVisibility::WorldReadable
		});

	pin_mut!(is_joined, is_world_readable);
	if !is_joined.or(is_world_readable).await {
		return Err!(Request(Forbidden("You aren't a member of the room.")));
	}

	Ok(joined_members::v3::Response {
		joined: services
			.state_accessor
			.room_state_full(&body.room_id)
			.ready_filter_map(Result::ok)
			.ready_filter(|((ty, _), _)| *ty == StateEventType::RoomMember)
			.map(at!(1))
			.ready_filter_map(|pdu| {
				let content = pdu.get_content::<RoomMemberEventContent>().ok()?;

				let matches = content.membership == MembershipState::Join;

				matches.then(|| {
					let sender = pdu.sender().to_owned();
					let member = RoomMember {
						display_name: content.displayname,
						avatar_url: content.avatar_url,
					};

					(sender, member)
				})
			})
			.collect()
			.boxed()
			.await,
	})
}
