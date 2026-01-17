use axum::extract::State;
use futures::{FutureExt, StreamExt, pin_mut};
use ruma::{
	UserId,
	api::client::user_directory::search_users::{self},
	events::room::join_rules::JoinRule,
};
use tuwunel_core::{
	Result,
	utils::{
		BoolExt, FutureBoolExt,
		stream::{BroadbandExt, ReadyExt},
	},
};
use tuwunel_service::Services;

use crate::Ruma;

// Tuwunel can handle a lot more results than synapse
const LIMIT_MAX: usize = 500;
const LIMIT_DEFAULT: usize = 10;

/// # `POST /_matrix/client/r0/user_directory/search`
///
/// Searches all known users for a match.
///
/// - Hides any local users that aren't in any public rooms (i.e. those that
///   have the join rule set to public) and don't share a room with the sender
pub(crate) async fn search_users_route(
	State(services): State<crate::State>,
	body: Ruma<search_users::v3::Request>,
) -> Result<search_users::v3::Response> {
	let sender_user = body.sender_user();
	let limit = usize::try_from(body.limit)
		.unwrap_or(LIMIT_DEFAULT)
		.min(LIMIT_MAX);

	let search_term = body.search_term.to_lowercase();
	let users = services
		.users
		.stream()
		.ready_filter(|&user_id| user_id != sender_user)
		.map(ToOwned::to_owned)
		.broad_filter_map(async |user_id| {
			let display_name = services.users.displayname(&user_id).await.ok();

			should_show_user(
				&services,
				sender_user,
				&user_id,
				display_name.as_deref(),
				&search_term,
			)
			.await
			.then_async(async || search_users::v3::User {
				user_id: user_id.clone(),
				display_name,
				avatar_url: services.users.avatar_url(&user_id).await.ok(),
			})
			.await
		});

	pin_mut!(users);
	let results = users.by_ref().take(limit).collect().await;
	let limited = users.next().await.is_some();

	Ok(search_users::v3::Response { results, limited })
}

async fn should_show_user(
	services: &Services,
	sender_user: &UserId,
	target_user: &UserId,
	target_display_name: Option<&str>,
	search_term: &str,
) -> bool {
	let user_id_matches = target_user
		.as_str()
		.to_lowercase()
		.contains(search_term);

	let display_name_matches = target_display_name
		.map(str::to_lowercase)
		.is_some_and(|display_name| display_name.contains(search_term));

	if !user_id_matches && !display_name_matches {
		return false;
	}

	if services
		.server
		.config
		.show_all_local_users_in_user_directory
	{
		return true;
	}

	let user_in_public_room = services
		.state_cache
		.rooms_joined(target_user)
		.map(ToOwned::to_owned)
		.broad_any(async |room_id| {
			services
				.state_accessor
				.get_join_rules(&room_id)
				.map(|rule| matches!(rule, JoinRule::Public))
				.await
		});

	let user_sees_user = services
		.state_cache
		.user_sees_user(sender_user, target_user);

	pin_mut!(user_in_public_room, user_sees_user);
	user_in_public_room.or(user_sees_user).await
}
