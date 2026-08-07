use std::collections::BTreeMap;

use futures::{FutureExt, StreamExt, TryFutureExt};
use ruma::{
	api::client::sync::sync_events::v5::response,
	events::typing::{SyncTypingEvent, TypingEventContent},
	serde::Raw,
};
use tuwunel_core::{
	Result, debug_error,
	utils::{IterStream, stream::BroadbandExt},
};

use super::{Connection, SyncInfo, Window, selector};

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

	selector(conn, window, implicit, explicit)
		.stream()
		.broad_filter_map(async |room_id| {
			let users = services
				.typing
				.typing_users_for_user(room_id, sender_user)
				.inspect_err(|e| debug_error!(%room_id, "Failed to get typing events: {e}"))
				.await
				.ok()
				.filter(|users| !users.is_empty())?;

			let content = TypingEventContent::new(users);
			let event = SyncTypingEvent { content };
			let event = Raw::new(&event);

			event
				.ok()
				.map(|event| (room_id.to_owned(), event))
		})
		.collect::<BTreeMap<_, _>>()
		.map(|rooms| Typing { rooms })
		.map(Ok)
		.await
}
