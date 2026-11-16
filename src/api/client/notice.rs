use futures::{TryFutureExt, future::ready};
use ruma::{
	RoomId, UserId,
	events::{StateEventType, tag::TagName},
};
use tuwunel_core::{Result, utils::BoolExt};
use tuwunel_matrix::Event;
use tuwunel_service::Services;

/// Tests whether a room is the target user's server-notice room.
///
/// The server creates the admin room and tags it for each admin, so it is
/// excluded after the marker check, which spares an ordinary room the
/// admin-room lookup. A failed marker read on the admin room answers false
/// rather than an error.
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) async fn is_notice_room(
	services: &Services,
	target: &UserId,
	room_id: &RoomId,
) -> Result<bool> {
	let notice = room_is_notice(services, target, room_id)
		.or_else(async |error| {
			services
				.admin
				.is_admin_room(room_id)
				.await
				.then_ok_or(false, error)
		})
		.await?;

	Ok(notice && !services.admin.is_admin_room(room_id).await)
}

/// Checks server membership, the target's tag, and the immutable room creator.
///
/// The tag is `m.server_notice`, or the admin room's configured tag on a room
/// an earlier release created. The admin room passes as well, so a caller must
/// exclude it. An absent tag is an ordinary non-notice room; other lookup
/// failures propagate.
#[tracing::instrument(level = "trace", skip_all)]
pub async fn room_is_notice(
	services: &Services,
	target: &UserId,
	room_id: &RoomId,
) -> Result<bool> {
	if !services
		.state_cache
		.is_joined(&services.globals.server_user, room_id)
		.await
	{
		return Ok(false);
	}

	let tagged = services
		.account_data
		.get_room_tags(target, room_id)
		.map_ok(|tags| {
			tags.keys()
				.any(|tag| is_notice_tag(tag, &services.config.admin.room.tag))
		})
		.or_else(|error| ready(error.is_not_found().then_ok_or(false, error)))
		.await?;

	if !tagged {
		return Ok(false);
	}

	services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomCreate, "")
		.map_ok(|create| create.sender() == services.globals.server_user)
		.await
}

/// Reports whether a room tag marks a server-notice room.
///
/// Notice rooms are tagged `m.server_notice`, but earlier releases tagged them
/// with the admin room's configured tag instead, so a non-empty `admin_tag`
/// still marks the rooms they created.
fn is_notice_tag(tag: &TagName, admin_tag: &str) -> bool {
	*tag == TagName::ServerNotice || (!admin_tag.is_empty() && tag.as_ref() == admin_tag)
}

#[cfg(test)]
mod tests {
	use ruma::events::tag::TagName;

	use super::is_notice_tag;

	#[test]
	fn notice_rooms_keep_the_admin_tag_earlier_releases_gave_them() {
		let custom = TagName::from("u.custom");

		assert!(is_notice_tag(&TagName::ServerNotice, "u.custom"));
		assert!(is_notice_tag(&TagName::ServerNotice, ""));
		assert!(is_notice_tag(&custom, "u.custom"));
		assert!(!is_notice_tag(&custom, "m.server_notice"));
		assert!(!is_notice_tag(&TagName::from(""), ""));
	}
}
