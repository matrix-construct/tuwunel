use futures::StreamExt;
use ruma::OwnedRoomId;
use tuwunel_core::{Err, Result};

use crate::{PAGE_SIZE, admin_command, get_room_info};

#[admin_command]
pub(super) async fn room_list(
	&self,
	page: Option<usize>,
	exclude_disabled: bool,
	exclude_banned: bool,
	no_details: bool,
) -> Result {
	// TODO: i know there's a way to do this with clap, but i can't seem to find it
	let page = page.unwrap_or(1);
	let mut rooms = self
		.services
		.metadata
		.iter_ids()
		.filter_map(async |room_id| {
			(!exclude_disabled || !self.services.metadata.is_disabled(room_id).await)
				.then_some(room_id)
		})
		.filter_map(async |room_id| {
			(!exclude_banned || !self.services.metadata.is_banned(room_id).await)
				.then_some(room_id)
		})
		.then(|room_id| get_room_info(self.services, room_id))
		.collect::<Vec<_>>()
		.await;

	rooms.sort_by_key(|r| r.1);
	rooms.reverse();

	let rooms = rooms
		.into_iter()
		.skip(page.saturating_sub(1).saturating_mul(PAGE_SIZE))
		.take(PAGE_SIZE)
		.collect::<Vec<_>>();

	if rooms.is_empty() {
		return Err!("No more rooms.");
	}

	let body = rooms
		.iter()
		.map(|(id, members, name)| {
			if no_details {
				format!("{id}")
			} else {
				format!("{id}\tMembers: {members}\tName: {name}")
			}
		})
		.collect::<Vec<_>>()
		.join("\n");

	self.write_str(&format!("Rooms ({}):\n```\n{body}\n```", rooms.len(),))
		.await
}

#[admin_command]
pub(super) async fn room_exists(&self, room_id: OwnedRoomId) -> Result {
	let result = self.services.metadata.exists(&room_id).await;

	self.write_str(&format!("{result}")).await
}

#[admin_command]
pub(super) async fn room_delete(&self, room_id: OwnedRoomId, force: bool) -> Result {
	if self.services.admin.is_admin_room(&room_id).await {
		return Err!("Cannot delete admin room");
	}

	let state_lock = self.services.state.mutex.lock(&room_id).await;

	self.services
		.delete
		.delete_room(&room_id, force, state_lock)
		.await?;

	self.write_str("Successfully deleted the room from our database.")
		.await?;

	Ok(())
}
