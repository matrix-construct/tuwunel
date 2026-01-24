use clap::Subcommand;
use futures::{FutureExt, StreamExt};
use ruma::{OwnedRoomId, OwnedRoomOrAliasId, RoomId, RoomOrAliasId};
use tuwunel_core::{
	Err, Result, debug, is_equal_to,
	utils::{IterStream, ReadyExt},
	warn,
};
use tuwunel_service::Services;

use crate::{admin_command, admin_command_dispatch, get_room_info};

#[admin_command_dispatch]
#[derive(Debug, Subcommand)]
pub(crate) enum RoomModerationCommand {
	/// - Bans a room from local users joining and evicts all our local users
	///   (including server
	/// admins)
	///   from the room. Also blocks any invites (local and remote) for the
	///   banned room, and disables federation entirely with it.
	BanRoom {
		/// The room in the format of `!roomid:example.com` or a room alias in
		/// the format of `#roomalias:example.com`
		room: OwnedRoomOrAliasId,
	},

	/// - Bans a list of rooms (room IDs and room aliases) from a newline
	///   delimited codeblock similar to `user deactivate-all`. Applies the same
	///   steps as ban-room
	BanListOfRooms,

	/// - Unbans a room to allow local users to join again
	UnbanRoom {
		/// The room in the format of `!roomid:example.com` or a room alias in
		/// the format of `#roomalias:example.com`
		room: OwnedRoomOrAliasId,
	},

	/// - List of all rooms we have banned
	ListBannedRooms {
		#[arg(long)]
		/// Whether to only output room IDs without supplementary room
		/// information
		no_details: bool,
	},
}

async fn do_ban_room(services: &Services, room_id: &RoomId) {
	services.metadata.ban_room(room_id);

	debug!("Banned {room_id} successfully");

	debug!("Making all users leave the room {room_id} and forgetting it");
	let mut users = services
		.state_cache
		.room_members(room_id)
		.ready_filter(|user| services.globals.user_is_local(user))
		.map(ToOwned::to_owned)
		.boxed();

	while let Some(ref user_id) = users.next().await {
		debug!(
			"Attempting leave for user {user_id} in room {room_id} (ignoring all errors, \
			 evicting admins too)",
		);

		let state_lock = services.state.mutex.lock(room_id).await;

		if let Err(e) = services
			.membership
			.leave(user_id, room_id, None, false, &state_lock)
			.boxed()
			.await
		{
			warn!("Failed to leave room: {e}");
		}

		drop(state_lock);

		services.state_cache.forget(room_id, user_id);
	}

	// remove any local aliases, ignore errors
	services
		.alias
		.local_aliases_for_room(room_id)
		.map(ToOwned::to_owned)
		.for_each(async |local_alias| {
			if let Err(e) = services.alias.remove_alias(&local_alias).await {
				warn!("Error removing alias {local_alias} for {room_id}: {e}");
			}
		})
		.await;

	// unpublish from room directory, ignore errors
	services.directory.set_not_public(room_id);

	services.metadata.disable_room(room_id);
}

#[admin_command]
async fn ban_room(&self, room: OwnedRoomOrAliasId) -> Result {
	debug!("Got room alias or ID: {}", room);

	let admin_room_alias = &self.services.admin.admin_alias;

	if let Ok(admin_room_id) = self.services.admin.get_admin_room().await
		&& (room.to_string().eq(&admin_room_id) || room.to_string().eq(admin_room_alias))
	{
		return Err!("Not allowed to ban the admin room.");
	}

	let room_id = self.services.alias.maybe_resolve(&room).await?;

	do_ban_room(self.services, &room_id).await;

	self.write_str(
		"Room banned, removed all our local users, and disabled incoming federation with room.",
	)
	.await
}

#[admin_command]
async fn ban_list_of_rooms(&self) -> Result {
	if self.body.len() < 2
		|| !self.body[0].trim().starts_with("```")
		|| self.body.last().unwrap_or(&"").trim() != "```"
	{
		return Err!("Expected code block in command body. Add --help for details.",);
	}

	let rooms_s = self
		.body
		.to_vec()
		.drain(1..self.body.len().saturating_sub(1))
		.collect::<Vec<_>>();

	let admin_room_id = self.services.admin.get_admin_room().await.ok();

	let mut room_ids: Vec<OwnedRoomId> = Vec::with_capacity(rooms_s.len());

	for room in rooms_s {
		let room_alias_or_id = match <&RoomOrAliasId>::try_from(room) {
			| Ok(room_alias_or_id) => room_alias_or_id,
			| Err(e) => {
				warn!("Error parsing room {room} during bulk room banning, ignoring: {e}");
				continue;
			},
		};

		let room_id = match self
			.services
			.alias
			.maybe_resolve(room_alias_or_id)
			.await
		{
			| Ok(room_id) => room_id,
			| Err(e) => {
				warn!("Failed to resolve room alias {room_alias_or_id} to a room ID: {e}");
				continue;
			},
		};

		if admin_room_id
			.as_ref()
			.is_some_and(is_equal_to!(&room_id))
		{
			warn!("User specified admin room in bulk ban list, ignoring");
			continue;
		}

		room_ids.push(room_id);
	}

	let rooms_len = room_ids.len();

	for room_id in room_ids {
		do_ban_room(self.services, &room_id).await;
	}

	self.write_str(&format!(
		"Finished bulk room ban, banned {rooms_len} total rooms, evicted all users, and \
		 disabled incoming federation with the room."
	))
	.await
}

#[admin_command]
async fn unban_room(&self, room: OwnedRoomOrAliasId) -> Result {
	let room_id = self.services.alias.maybe_resolve(&room).await?;

	self.services.metadata.unban_room(&room_id);
	self.services.metadata.enable_room(&room_id);
	self.write_str("Room unbanned and federation re-enabled.")
		.await
}

#[admin_command]
async fn list_banned_rooms(&self, no_details: bool) -> Result {
	let room_ids: Vec<OwnedRoomId> = self
		.services
		.metadata
		.list_banned_rooms()
		.map(Into::into)
		.collect()
		.await;

	if room_ids.is_empty() {
		return Err!("No rooms are banned.");
	}

	let mut rooms = room_ids
		.iter()
		.stream()
		.then(|room_id| get_room_info(self.services, room_id))
		.collect::<Vec<_>>()
		.await;

	rooms.sort_by_key(|r| r.1);
	rooms.reverse();

	let num = rooms.len();

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

	self.write_str(&format!("Rooms Banned ({num}):\n```\n{body}\n```",))
		.await
}
