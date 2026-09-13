use futures::StreamExt;
use tuwunel_core::{Result, itertools::Itertools};

use crate::{PAGE_SIZE, admin_command, get_room_info};

#[admin_command]
pub(super) async fn directory_list(&self, page: Option<usize>) -> Result {
	let page = page.unwrap_or(1);
	let rooms: Vec<_> = self
		.services
		.directory
		.public_rooms()
		.then(|room_id| get_room_info(self.services, room_id))
		.collect::<Vec<_>>()
		.await
		.into_iter()
		.sorted_by_key(|r| r.1)
		.rev()
		.skip(page.saturating_sub(1).saturating_mul(PAGE_SIZE))
		.take(PAGE_SIZE)
		.collect();

	if rooms.is_empty() {
		self.write_str("No rooms are published.").await?;

		return Ok(());
	}

	write!(self, "Rooms (page {page}):\n```\n").await?;
	for (id, members, name) in &rooms {
		writeln!(self, "{id} | Members: {members} | Name: {name}").await?;
	}
	write!(self, "```").await
}
