use futures::{StreamExt, TryStreamExt};
use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
pub(super) async fn room_prune_empty(&self, force: bool) -> Result {
	let config = &self.services.server.config.rooms;
	let require_forgotten = config.prune_require_forgotten && !force;
	let rooms_len = self
		.services
		.metadata
		.iter_ids()
		.map(ToOwned::to_owned)
		.map(Ok)
		// An eligibility failure must not cancel another room midway through its purge.
		.try_fold(0_usize, async |count, room_id| -> Result<usize> {
			let state_lock = self.services.state.mutex.lock(&room_id).await;

			if !self
				.services
				.state_cache
				.can_prune_room(&room_id, require_forgotten)
				.await?
			{
				return Ok(count);
			}

			self.services
				.delete
				.delete_room(&room_id, force, state_lock)
				.await?;

			Ok(count.saturating_add(1))
		})
		.await?;

	write!(self, "Successfully deleted {rooms_len} rooms from our database.").await?;

	Ok(())
}
