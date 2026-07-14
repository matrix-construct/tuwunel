use std::borrow::Cow;

use ruma::OwnedRoomOrAliasId;
use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
#[tracing::instrument(level = "debug", skip(self))]
pub(super) async fn delete_forward_extremities(&self, room: OwnedRoomOrAliasId) -> Result {
	let room_id = self.services.alias.maybe_resolve(&room).await?;

	let state_lock = self.services.state.mutex.lock(&room_id).await;

	let deleted = self
		.services
		.state
		.collapse_forward_extremities(&room_id, &state_lock)
		.await;

	let out: Cow<'_, str> = match deleted {
		| 0 => "The room has one or no forward extremities; nothing to prune.".into(),
		| 1 => "Pruned 1 forward extremity, leaving one.".into(),
		| _ => format!("Pruned {deleted} forward extremities, leaving one.").into(),
	};

	self.write_str(&out).await
}
