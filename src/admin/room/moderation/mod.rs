mod ban_list_of_rooms;
mod ban_room;
mod list_banned_rooms;
mod unban_room;

use clap::Subcommand;
use futures::StreamExt;
use ruma::{OwnedRoomOrAliasId, RoomId};
use tuwunel_core::{Result, debug, utils::ReadyExt, warn};
use tuwunel_service::Services;

use crate::admin_command_dispatch;

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
