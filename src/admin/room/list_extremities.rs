use futures::{StreamExt, TryStreamExt};
use ruma::OwnedRoomOrAliasId;
use tuwunel_core::{Error, Result};

use crate::admin_command;

#[admin_command]
#[tracing::instrument(level = "debug", skip(self))]
pub(super) async fn room_list_extremities(&self, room_id: OwnedRoomOrAliasId) -> Result {
	let room_id = self
		.services
		.alias
		.maybe_resolve(&room_id)
		.await?;

	write!(self, "Forward extremities for {room_id}:\n```\n").await?;

	let total = self
		.services
		.state
		.get_forward_extremities(&room_id)
		.map(Ok::<_, Error>)
		.try_fold(0_usize, async |count, event_id| {
			writeln!(self, "{event_id}").await?;

			Ok(count.saturating_add(1))
		})
		.await?;

	write!(self, "```\nTotal: {total}").await
}
