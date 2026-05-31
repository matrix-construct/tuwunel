use std::cmp::Ordering;

use futures::{StreamExt, future::join};
use ruma::{
	MxcUri, OwnedMxcUri, RoomId, UserId,
	api::client::sync::sync_events::v5::{DisplayName, response, response::Heroes},
};
use tuwunel_core::utils::{BoolExt, ReadyExt, TryFutureExtExt, stream::BroadbandExt};
use tuwunel_service::Services;

const MAX_HEROES: usize = 5;

#[tracing::instrument(name = "heroes", level = "trace", skip_all)]
pub(super) async fn calculate_heroes(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	room_name: Option<&DisplayName>,
	room_avatar: Option<&MxcUri>,
) -> (Option<Heroes>, Option<DisplayName>, Option<OwnedMxcUri>) {
	let heroes: Heroes = services
		.state_cache
		.room_members(room_id)
		.ready_filter(|&member| member != sender_user)
		.ready_filter_map(|member| room_name.is_none().then_some(member))
		.map(ToOwned::to_owned)
		.broadn_filter_map(MAX_HEROES, async |user_id| {
			let content = services
				.state_accessor
				.get_member(room_id, &user_id)
				.await
				.ok()?;

			let name = content
				.displayname
				.is_none()
				.then_async(|| services.profile.displayname(&user_id).ok());

			let avatar = content
				.avatar_url
				.is_none()
				.then_async(|| services.profile.avatar_url(&user_id).ok());

			let (name, avatar) = join(name, avatar).await;
			let hero = response::Hero {
				user_id,
				avatar: avatar.unwrap_or(content.avatar_url),
				name: name
					.unwrap_or(content.displayname)
					.map(Into::into),
			};

			Some(hero)
		})
		.take(MAX_HEROES)
		.collect()
		.await;

	let hero_name = match heroes.len().cmp(&(1_usize)) {
		| Ordering::Less => None,
		| Ordering::Equal => Some(
			heroes[0]
				.name
				.clone()
				.unwrap_or_else(|| heroes[0].user_id.as_str().into()),
		),
		| Ordering::Greater => {
			let firsts = heroes[1..]
				.iter()
				.map(|h| {
					h.name
						.clone()
						.unwrap_or_else(|| h.user_id.as_str().into())
				})
				.collect::<Vec<_>>()
				.join(", ");

			let last = heroes[0]
				.name
				.clone()
				.unwrap_or_else(|| heroes[0].user_id.as_str().into());

			Some(format!("{firsts} and {last}")).map(Into::into)
		},
	};

	let heroes_avatar = (room_avatar.is_none() && room_name.is_none())
		.then(|| {
			heroes
				.first()
				.and_then(|hero| hero.avatar.clone())
		})
		.flatten();

	(Some(heroes), hero_name, heroes_avatar)
}
