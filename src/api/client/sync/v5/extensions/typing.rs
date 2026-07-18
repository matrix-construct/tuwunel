use std::collections::BTreeMap;

use futures::{FutureExt, StreamExt, TryFutureExt};
use ruma::{
	OwnedRoomId, RoomId,
	api::client::sync::sync_events::v5::response,
	events::typing::{SyncTypingEvent, TypingEventContent},
	serde::Raw,
};
use tuwunel_core::{
	Result, debug_error,
	utils::{IterStream, ReadyExt},
};

use super::{Connection, SyncInfo, Window, selector};

#[cfg(test)]
mod tests;

#[tracing::instrument(name = "typing", level = "trace", skip_all, ret)]
pub(super) async fn collect(
	sync_info: SyncInfo<'_>,
	conn: &Connection,
	window: &Window,
) -> Result<response::Typing> {
	use response::Typing;

	let SyncInfo { services, sender_user, .. } = sync_info;

	let implicit = conn
		.extensions
		.typing
		.lists
		.as_deref()
		.map(<[_]>::iter);

	let explicit = conn
		.extensions
		.typing
		.rooms
		.as_deref()
		.map(<[_]>::iter);

	selector(sync_info, conn, window, implicit, explicit)
		.stream()
		.filter_map(async |room_id| {
			if !services
				.state_cache
				.is_joined(sender_user, room_id)
				.await
			{
				return None;
			}

			let last_typing_update = services
				.typing
				.last_typing_update(room_id)
				.inspect_err(|e| debug_error!(%room_id, "Failed to get typing update count: {e}"))
				.await
				.ok()?;

			if last_typing_update <= conn.globalsince || last_typing_update > conn.next_batch {
				return None;
			}

			services
				.typing
				.typing_users_for_user(room_id, sender_user)
				.inspect_err(|e| debug_error!(%room_id, "Failed to get typing events: {e}"))
				.await
				.ok()
				.filter(|users| conn.globalsince != 0 || !users.is_empty())
				.map(|users| (room_id, users))
		})
		.ready_filter_map(|(room_id, users)| room_typing_event(room_id, users))
		.collect::<BTreeMap<_, _>>()
		.map(|rooms| Typing { rooms })
		.map(Ok)
		.await
}

fn room_typing_event(
	room_id: &RoomId,
	users: Vec<ruma::OwnedUserId>,
) -> Option<(OwnedRoomId, Raw<SyncTypingEvent>)> {
	let content = TypingEventContent::new(users);
	let event = SyncTypingEvent { content };
	let event = Raw::new(&event);

	Some((room_id.to_owned(), event.ok()?))
}
