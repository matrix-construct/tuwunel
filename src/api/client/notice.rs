use futures::{FutureExt, TryFutureExt, future::ready};
use ruma::{
	RoomId, UserId,
	events::{StateEventType, tag::TagName},
};
use tuwunel_core::{Result, utils::BoolExt};
use tuwunel_matrix::Event;
use tuwunel_service::Services;

/// Tests whether a room is the target user's server-notice room.
///
/// The configured tag and global server identity are applied consistently with
/// notice-room lookup and creation.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) async fn is_notice_room(
	services: &Services,
	target: &UserId,
	room_id: &RoomId,
) -> Result<bool> {
	let server_user = services.globals.server_user.as_ref();
	let tag = notice_tag(&services.config.admin_room_tag);

	if services
		.admin
		.get_admin_room()
		.map(|admin| admin.is_ok_and(|admin| admin == room_id))
		.await
	{
		return Ok(false);
	}

	room_is_notice(services, server_user, target, &tag, room_id).await
}

/// Checks server membership, the target's tag, and the immutable room creator.
///
/// An absent tag is an ordinary non-notice room; other lookup failures propagate.
#[tracing::instrument(level = "trace", skip_all)]
pub async fn room_is_notice(
	services: &Services,
	server_user: &UserId,
	target: &UserId,
	tag: &TagName,
	room_id: &RoomId,
) -> Result<bool> {
	if !services
		.state_cache
		.is_joined(server_user, room_id)
		.await
	{
		return Ok(false);
	}

	let tagged = services
		.account_data
		.get_room_tags(target, room_id)
		.map_ok(|tags| tags.contains_key(tag))
		.or_else(|error| ready(error.is_not_found().then_ok_or(false, error)))
		.await?;

	if !tagged {
		return Ok(false);
	}

	services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomCreate, "")
		.map_ok(|create| is_notice_creator(create.sender(), server_user))
		.await
}

/// Selects the configured notice tag, falling back when it is empty.
///
/// The fallback matches the tag used when creating server-notice rooms.
pub fn notice_tag(tag: &str) -> TagName {
	Some(tag)
		.filter(|tag| !tag.is_empty())
		.map_or(TagName::ServerNotice, Into::into)
}

/// Matches the immutable create-event sender to the configured server identity.
///
/// Membership alone cannot distinguish notice rooms from ordinary shared rooms.
fn is_notice_creator(create_sender: &UserId, server_user: &UserId) -> bool {
	create_sender == server_user
}

#[cfg(test)]
mod tests {
	use ruma::{events::tag::TagName, user_id};

	use super::{is_notice_creator, notice_tag};

	#[test]
	fn only_the_server_user_creates_a_notice_room() {
		let server = user_id!("@server:example.com");
		let other = user_id!("@other:example.com");

		assert!(is_notice_creator(server, server));
		assert!(!is_notice_creator(other, server));
	}

	#[test]
	fn empty_config_tag_falls_back_to_server_notice() {
		assert_eq!(notice_tag(""), TagName::ServerNotice);
		assert_eq!(notice_tag("m.server_notice"), TagName::ServerNotice);
		assert_eq!(notice_tag("u.custom"), TagName::from("u.custom"));
	}
}
