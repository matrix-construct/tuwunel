use futures::StreamExt;
use ruma::OwnedRoomId;
use tuwunel_core::{Result, utils::ReadyExt};

use crate::admin_command;

#[admin_command]
pub(super) async fn list_joined_members(&self, room_id: OwnedRoomId, local_only: bool) -> Result {
	let room_name = self
		.services
		.state_accessor
		.get_name(&room_id)
		.await
		.unwrap_or_else(|_| room_id.to_string());

	let member_info: Vec<_> = self
		.services
		.state_cache
		.room_members(&room_id)
		.ready_filter(|user_id| {
			local_only
				.then(|| self.services.globals.user_is_local(user_id))
				.unwrap_or(true)
		})
		.map(ToOwned::to_owned)
		.filter_map(async |user_id| {
			Some((
				self.services
					.profile
					.displayname(&user_id)
					.await
					.unwrap_or_else(|_| user_id.to_string()),
				user_id,
			))
		})
		.collect()
		.await;

	let num = member_info.len();
	write!(self, "{num} Members in Room \"{room_name}\":\n```\n").await?;
	for (displayname, mxid) in &member_info {
		writeln!(self, "{mxid} | {displayname}").await?;
	}
	write!(self, "```").await
}
