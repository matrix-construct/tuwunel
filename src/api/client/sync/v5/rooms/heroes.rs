use std::cmp::Ordering;

use futures::{Stream, StreamExt, future::join};
use ruma::{
	MxcUri, OwnedMxcUri, OwnedUserId, RoomId, UserId,
	api::client::sync::sync_events::v5::{
		DisplayName,
		response::{Hero, Heroes},
	},
};
use tuwunel_core::utils::{BoolExt, ReadyExt, TryFutureExtExt, stream::WidebandExt};
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
	let heroes: Heroes = room_name
		.is_none()
		.then_async(|| hero_members(services, sender_user, room_id))
		.await
		.unwrap_or_default();

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

async fn hero_members(services: &Services, sender_user: &UserId, room_id: &RoomId) -> Heroes {
	let members = services
		.state_cache
		.room_members(room_id)
		.ready_filter(|&member| member != sender_user)
		.map(ToOwned::to_owned);

	first_heroes(members, async |user_id| {
		let content = services
			.state_accessor
			.get_member(room_id, &user_id)
			.ok()
			.await?;

		let name = content
			.displayname
			.is_none()
			.then_async(|| services.profile.displayname(&user_id).ok());

		let avatar = content
			.avatar_url
			.is_none()
			.then_async(|| services.profile.avatar_url(&user_id).ok());

		let (name, avatar) = join(name, avatar).await;
		let name = name
			.unwrap_or(content.displayname)
			.map(Into::into);

		let hero = Hero {
			user_id,
			avatar: avatar.unwrap_or(content.avatar_url),
			name,
		};

		Some(hero)
	})
	.collect()
	.await
}

/// Yields the first heroes `lookup` finds, in member order.
///
/// A room's name and avatar derive from this list, so which members lead it
/// must not depend on which lookup finished first.
fn first_heroes<Members, Lookup, Found>(
	members: Members,
	lookup: Lookup,
) -> impl Stream<Item = Hero> + Send
where
	Members: Stream<Item = OwnedUserId> + Send,
	Lookup: Fn(OwnedUserId) -> Found + Send,
	Found: Future<Output = Option<Hero>> + Send,
{
	members
		.widen_filter_map(MAX_HEROES, lookup)
		.take(MAX_HEROES)
}

#[cfg(test)]
mod tests {
	use ruma::user_id;
	use tokio::task::yield_now;
	use tuwunel_core::utils::IterStream;

	use super::*;

	#[tokio::test]
	async fn first_heroes_keep_member_order_past_slow_and_failed_lookups() {
		let members = [
			user_id!("@a:example.com"),
			user_id!("@b:example.com"),
			user_id!("@c:example.com"),
			user_id!("@d:example.com"),
			user_id!("@e:example.com"),
			user_id!("@f:example.com"),
			user_id!("@g:example.com"),
		];

		let [slow, failed, ..] = members;
		let heroes: Vec<_> =
			first_heroes(members.map(ToOwned::to_owned).stream(), async |user_id| {
				if user_id == slow {
					yield_now().await;
				}

				user_id
					.ne(failed)
					.then_some(Hero { user_id, avatar: None, name: None })
			})
			.map(|hero| hero.user_id)
			.collect()
			.await;

		let expected = [members[0], members[2], members[3], members[4], members[5]];

		assert_eq!(heroes, expected.map(ToOwned::to_owned));
	}
}
