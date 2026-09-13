use futures::StreamExt;
use tuwunel_core::{Err, Result, itertools::Itertools};

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
	let rooms = self
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
		.await
		.into_iter()
		.sorted_by_key(|r| r.1)
		.rev()
		.skip(page.saturating_sub(1).saturating_mul(PAGE_SIZE))
		.take(PAGE_SIZE)
		.collect::<Vec<_>>();

	if rooms.is_empty() {
		return Err!("No more rooms.");
	}

	write!(self, "Rooms ({}):\n```\n", rooms.len()).await?;
	for (id, members, name) in &rooms {
		if no_details {
			writeln!(self, "{id}").await?;
		} else {
			writeln!(self, "{id}\tMembers: {members}\tName: {name}").await?;
		}
	}
	write!(self, "```").await
}
