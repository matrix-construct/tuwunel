use futures::{StreamExt, pin_mut};
use ruma::{
	RoomId, api::client::sync::sync_events::v5::request::ListFilters, directory::RoomTypeFilter,
	events::room::member::MembershipState,
};
use tuwunel_core::{
	is_equal_to, is_true,
	utils::{
		BoolExt, FutureBoolExt, IterStream, ReadyExt,
		future::{self, OptionFutureExt, ReadyBoolExt},
		option::OptionExt,
	},
};

use super::SyncInfo;

#[tracing::instrument(name = "filter", level = "trace", skip_all)]
pub(super) async fn filter_room(
	SyncInfo { services, sender_user, .. }: SyncInfo<'_>,
	filter: &ListFilters,
	room_id: &RoomId,
	membership: Option<&MembershipState>,
) -> bool {
	#[expect(clippy::match_same_arms)] // helps readability
	let match_invite = filter
		.is_invite
		.map_async(async |is_invite| match (membership, is_invite) {
			| (Some(MembershipState::Invite), true) => true,
			| (Some(MembershipState::Invite), false) => false,
			| (Some(_), true) => false,
			| (Some(_), false) => true,
			| _ =>
				services
					.state_cache
					.is_invited(sender_user, room_id)
					.await == is_invite,
		});

	let match_direct = filter.is_dm.map_async(async |is_dm| {
		services
			.account_data
			.is_direct(sender_user, room_id)
			.await == is_dm
	});

	let match_direct_member = filter.is_dm.map_async(async |is_dm| {
		services
			.state_accessor
			.is_direct(room_id, sender_user)
			.await == is_dm
	});

	let match_encrypted = filter
		.is_encrypted
		.map_async(async |is_encrypted| {
			services
				.state_accessor
				.is_encrypted_room(room_id)
				.await == is_encrypted
		});

	let match_space_child = filter
		.spaces
		.is_empty()
		.is_false()
		.then_async(async || {
			filter
				.spaces
				.iter()
				.stream()
				.flat_map(|room_id| services.spaces.get_space_children(room_id))
				.ready_any(is_equal_to!(room_id))
				.await
		});

	let fetch_tags = !filter.tags.is_empty() || !filter.not_tags.is_empty();
	let match_room_tag = fetch_tags.then_async(async || {
		if let Some(tags) = services
			.account_data
			.get_room_tags(sender_user, room_id)
			.await
			.ok()
			.filter(|tags| !tags.is_empty())
		{
			tags.keys().any(|tag| {
				(filter.not_tags.is_empty() || !filter.not_tags.contains(tag))
					|| (!filter.tags.is_empty() && filter.tags.contains(tag))
			})
		} else {
			filter.tags.is_empty()
		}
	});

	let fetch_room_type = !filter.room_types.is_empty() || !filter.not_room_types.is_empty();
	let match_room_type = fetch_room_type.then_async(async || {
		let room_type = services
			.state_accessor
			.get_room_type(room_id)
			.await
			.ok();

		let room_type = RoomTypeFilter::from(room_type);
		(filter.not_room_types.is_empty() || !filter.not_room_types.contains(&room_type))
			&& (filter.room_types.is_empty() || filter.room_types.contains(&room_type))
	});

	future::and7(
		match_invite.is_none_or(is_true!()),
		match_encrypted.is_none_or(is_true!()),
		match_direct.is_none_or(is_true!()),
		match_direct_member.is_none_or(is_true!()),
		match_space_child.is_none_or(is_true!()),
		match_room_type.is_none_or(is_true!()),
		match_room_tag.is_none_or(is_true!()),
	)
	.await
}

#[tracing::instrument(name = "filter_meta", level = "trace", skip_all)]
pub(super) async fn filter_room_meta(
	SyncInfo { services, sender_user, .. }: SyncInfo<'_>,
	room_id: &RoomId,
) -> bool {
	let not_exists = services.metadata.exists(room_id).is_false();

	let is_disabled = services.metadata.is_disabled(room_id);

	let is_banned = services.metadata.is_banned(room_id);

	let not_visible = services
		.state_accessor
		.user_can_see_state_events(sender_user, room_id)
		.is_false();

	let not_invited = services
		.state_cache
		.is_invited(sender_user, room_id)
		.is_false();

	pin_mut!(not_visible, not_invited, not_exists, is_disabled, is_banned);
	not_visible
		.and(not_invited)
		.or(not_exists)
		.or(is_disabled)
		.or(is_banned)
		.is_false()
		.await
}
