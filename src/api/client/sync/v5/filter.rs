use futures::StreamExt;
use ruma::{
	RoomId, UserId,
	api::client::sync::sync_events::v5::request::ListFilters,
	directory::RoomTypeFilter,
	events::{StateEventType, room::member::MembershipState, tag::TagName},
	room::RoomType,
};
use serde::{Deserialize, de::IgnoredAny};
use tuwunel_core::{
	is_equal_to, is_true,
	utils::{
		BoolExt, FutureBoolExt, IterStream, ReadyExt,
		future::{self, OptionFutureExt, ReadyBoolExt, ReadyEqExt},
		option::OptionExt,
	},
};
use tuwunel_service::{Services, rooms::state_cache::StrippedRoomState};

use super::{SyncInfo, stripped_content, stripped_state};

#[derive(Deserialize)]
struct CreateContent {
	#[serde(rename = "type")]
	room_type: Option<RoomType>,
}

#[tracing::instrument(name = "filter", level = "trace", skip_all)]
pub(super) async fn filter_room(
	SyncInfo { services, sender_user, direct_rooms, .. }: SyncInfo<'_>,
	filter: &ListFilters,
	room_id: &RoomId,
	membership: Option<&MembershipState>,
) -> bool {
	#[expect(clippy::match_same_arms)] // helps readability
	let invite_matches = async |is_invite| match (membership, is_invite) {
		| (Some(MembershipState::Invite), true) => true,
		| (Some(MembershipState::Invite), false) => false,
		| (Some(_), true) => false,
		| (Some(_), false) => true,
		| _ =>
			services
				.state_cache
				.is_invited(sender_user, room_id)
				.eq(&is_invite)
				.await,
	};

	let match_invite = filter.is_invite.map_async(invite_matches);

	let match_direct = filter
		.is_dm
		.is_none_or(is_equal_to!(direct_rooms.contains(room_id)));

	let pending_knock = membership == Some(&MembershipState::Knock);
	let match_encrypted = filter
		.is_encrypted
		.map_async(async |is_encrypted| {
			// Encryption is never turned off, so the knock summary can only add it.
			let encrypted = services
				.state_accessor
				.is_encrypted_room(room_id)
				.await
				|| (pending_knock && knock_encrypted(services, sender_user, room_id).await);

			encrypted == is_encrypted
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

	// MSC4186: `tags` is a union, and `not_tags` takes priority over it.
	let match_room_tag = fetch_tags.then_async(async || {
		let tags = services
			.account_data
			.get_room_tags(sender_user, room_id)
			.await
			.unwrap_or_default();

		let tagged = |names: &[TagName]| tags.keys().any(|tag| names.contains(tag));

		(filter.tags.is_empty() || tagged(&filter.tags)) && !tagged(&filter.not_tags)
	});

	let fetch_room_type = !filter.room_types.is_empty() || !filter.not_room_types.is_empty();
	let match_room_type = fetch_room_type.then_async(async || {
		let room_type = services
			.state_accessor
			.get_room_type(room_id)
			.await
			.ok();

		let room_type = match room_type {
			| None if pending_knock => knock_room_type(services, sender_user, room_id).await,
			| room_type => room_type,
		};

		let room_type = RoomTypeFilter::from(room_type);

		(filter.not_room_types.is_empty() || !filter.not_room_types.contains(&room_type))
			&& (filter.room_types.is_empty() || filter.room_types.contains(&room_type))
	});

	match_direct
		&& future::and5(
			match_invite.is_none_or(is_true!()),
			match_encrypted.is_none_or(is_true!()),
			match_space_child.is_none_or(is_true!()),
			match_room_type.is_none_or(is_true!()),
			match_room_tag.is_none_or(is_true!()),
		)
		.await
}

async fn knock_encrypted(services: &Services, sender_user: &UserId, room_id: &RoomId) -> bool {
	knock_summary(services, sender_user, room_id)
		.await
		.is_some_and(|summary| {
			let encryption: Option<IgnoredAny> =
				stripped_content(&summary, &StateEventType::RoomEncryption);

			encryption.is_some()
		})
}

async fn knock_room_type(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
) -> Option<RoomType> {
	let summary = knock_summary(services, sender_user, room_id).await?;

	stripped_content(&summary, &StateEventType::RoomCreate)
		.and_then(|content: CreateContent| content.room_type)
}

async fn knock_summary(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
) -> StrippedRoomState {
	stripped_state(services, sender_user, room_id, Some(&MembershipState::Knock)).await
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

	let not_once_joined = services
		.state_cache
		.once_joined(sender_user, room_id)
		.is_false();

	not_visible
		.and2(not_invited, not_once_joined)
		.or3(not_exists, is_disabled, is_banned)
		.is_false()
		.await
}
