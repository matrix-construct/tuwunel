use clap::Subcommand;
use futures::{FutureExt, StreamExt};
use ruma::{OwnedRoomId, OwnedRoomOrAliasId, RoomAliasId, RoomId, RoomOrAliasId};
use tuwunel_core::{
	Err, Result, debug,
	utils::{IterStream, ReadyExt},
	warn,
};

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

#[admin_command]
async fn ban_room(&self, room: OwnedRoomOrAliasId) -> Result {
	debug!("Got room alias or ID: {}", room);

	let admin_room_alias = &self.services.admin.admin_alias;

	if let Ok(admin_room_id) = self.services.admin.get_admin_room().await
		&& (room.to_string().eq(&admin_room_id) || room.to_string().eq(admin_room_alias))
	{
		return Err!("Not allowed to ban the admin room.");
	}

	let room_id = if room.is_room_id() {
		let room_id = match RoomId::parse(&room) {
			| Ok(room_id) => room_id,
			| Err(e) => {
				return Err!(
					"Failed to parse room ID {room}. Please note that this requires a full room \
					 ID (`!awIh6gGInaS5wLQJwa:example.com`) or a room alias \
					 (`#roomalias:example.com`): {e}"
				);
			},
		};

		debug!("Room specified is a room ID, banning room ID");
		self.services.metadata.ban_room(room_id);

		room_id.to_owned()
	} else if room.is_room_alias_id() {
		let room_alias = match RoomAliasId::parse(&room) {
			| Ok(room_alias) => room_alias,
			| Err(e) => {
				return Err!(
					"Failed to parse room ID {room}. Please note that this requires a full room \
					 ID (`!awIh6gGInaS5wLQJwa:example.com`) or a room alias \
					 (`#roomalias:example.com`): {e}"
				);
			},
		};

		debug!(
			"Room specified is not a room ID, attempting to resolve room alias to a room ID \
			 locally, if not using get_alias_helper to fetch room ID remotely"
		);

		let room_id = match self
			.services
			.alias
			.resolve_local_alias(room_alias)
			.await
		{
			| Ok(room_id) => room_id,
			| _ => {
				debug!(
					"We don't have this room alias to a room ID locally, attempting to fetch \
					 room ID over federation"
				);

				match self
					.services
					.alias
					.resolve_alias(room_alias)
					.await
				{
					| Ok((room_id, servers)) => {
						debug!(
							?room_id,
							?servers,
							"Got federation response fetching room ID for {room_id}"
						);
						room_id
					},
					| Err(e) => {
						return Err!(
							"Failed to resolve room alias {room_alias} to a room ID: {e}"
						);
					},
				}
			},
		};

		self.services.metadata.ban_room(&room_id);

		room_id
	} else {
		return Err!(
			"Room specified is not a room ID or room alias. Please note that this requires a \
			 full room ID (`!awIh6gGInaS5wLQJwa:example.com`) or a room alias \
			 (`#roomalias:example.com`)",
		);
	};

	debug!("Making all users leave the room {room_id} and forgetting it");
	let mut users = self
		.services
		.state_cache
		.room_members(&room_id)
		.map(ToOwned::to_owned)
		.ready_filter(|user| self.services.globals.user_is_local(user))
		.boxed();

	while let Some(ref user_id) = users.next().await {
		debug!(
			"Attempting leave for user {user_id} in room {room_id} (ignoring all errors, \
			 evicting admins too)",
		);

		let state_lock = self.services.state.mutex.lock(&room_id).await;

		if let Err(e) = self
			.services
			.membership
			.leave(user_id, &room_id, None, false, &state_lock)
			.boxed()
			.await
		{
			warn!("Failed to leave room: {e}");
		}

		drop(state_lock);

		self.services
			.state_cache
			.forget(&room_id, user_id);
	}

	self.services
		.alias
		.local_aliases_for_room(&room_id)
		.map(ToOwned::to_owned)
		.for_each(async |local_alias| {
			self.services
				.alias
				.remove_alias(&local_alias, &self.services.globals.server_user)
				.await
				.ok();
		})
		.await;

	// unpublish from room directory
	self.services.directory.set_not_public(&room_id);

	self.services.metadata.disable_room(&room_id);

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

	let admin_room_alias = &self.services.admin.admin_alias;

	let mut room_ban_count: usize = 0;
	let mut room_ids: Vec<OwnedRoomId> = Vec::new();

	for &room in &rooms_s {
		match <&RoomOrAliasId>::try_from(room) {
			| Ok(room_alias_or_id) => {
				if let Ok(admin_room_id) = self.services.admin.get_admin_room().await
					&& (room.to_owned().eq(&admin_room_id)
						|| room.to_owned().eq(admin_room_alias))
				{
					warn!("User specified admin room in bulk ban list, ignoring");
					continue;
				}

				if room_alias_or_id.is_room_id() {
					let room_id = match RoomId::parse(room_alias_or_id) {
						| Ok(room_id) => room_id,
						| Err(e) => {
							// ignore rooms we failed to parse
							warn!(
								"Error parsing room \"{room}\" during bulk room banning, \
								 ignoring error and logging here: {e}"
							);
							continue;
						},
					};

					room_ids.push(room_id.to_owned());
				}

				if room_alias_or_id.is_room_alias_id() {
					match RoomAliasId::parse(room_alias_or_id) {
						| Ok(room_alias) => {
							let room_id = match self
								.services
								.alias
								.resolve_local_alias(room_alias)
								.await
							{
								| Ok(room_id) => room_id,
								| _ => {
									debug!(
										"We don't have this room alias to a room ID locally, \
										 attempting to fetch room ID over federation"
									);

									match self
										.services
										.alias
										.resolve_alias(room_alias)
										.await
									{
										| Ok((room_id, servers)) => {
											debug!(
												?room_id,
												?servers,
												"Got federation response fetching room ID for \
												 {room}",
											);
											room_id
										},
										| Err(e) => {
											warn!(
												"Failed to resolve room alias {room} to a room \
												 ID: {e}"
											);
											continue;
										},
									}
								},
							};

							room_ids.push(room_id);
						},
						| Err(e) => {
							warn!(
								"Error parsing room \"{room}\" during bulk room banning, \
								 ignoring error and logging here: {e}"
							);
							continue;
						},
					}
				}
			},
			| Err(e) => {
				warn!(
					"Error parsing room \"{room}\" during bulk room banning, ignoring error and \
					 logging here: {e}"
				);
				continue;
			},
		}
	}

	for room_id in room_ids {
		self.services.metadata.ban_room(&room_id);

		debug!("Banned {room_id} successfully");
		room_ban_count = room_ban_count.saturating_add(1);

		debug!("Making all users leave the room {room_id} and forgetting it");
		let mut users = self
			.services
			.state_cache
			.room_members(&room_id)
			.map(ToOwned::to_owned)
			.ready_filter(|user| self.services.globals.user_is_local(user))
			.boxed();

		while let Some(ref user_id) = users.next().await {
			debug!(
				"Attempting leave for user {user_id} in room {room_id} (ignoring all errors, \
				 evicting admins too)",
			);

			let state_lock = self.services.state.mutex.lock(&room_id).await;

			if let Err(e) = self
				.services
				.membership
				.leave(user_id, &room_id, None, false, &state_lock)
				.boxed()
				.await
			{
				warn!("Failed to leave room: {e}");
			}

			drop(state_lock);

			self.services
				.state_cache
				.forget(&room_id, user_id);
		}

		// remove any local aliases, ignore errors
		self.services
			.alias
			.local_aliases_for_room(&room_id)
			.map(ToOwned::to_owned)
			.for_each(async |local_alias| {
				self.services
					.alias
					.remove_alias(&local_alias, &self.services.globals.server_user)
					.await
					.ok();
			})
			.await;

		// unpublish from room directory, ignore errors
		self.services.directory.set_not_public(&room_id);

		self.services.metadata.disable_room(&room_id);
	}

	self.write_str(&format!(
		"Finished bulk room ban, banned {room_ban_count} total rooms, evicted all users, and \
		 disabled incoming federation with the room."
	))
	.await
}

#[admin_command]
async fn unban_room(&self, room: OwnedRoomOrAliasId) -> Result {
	let room_id = if room.is_room_id() {
		let room_id = match RoomId::parse(&room) {
			| Ok(room_id) => room_id,
			| Err(e) => {
				return Err!(
					"Failed to parse room ID {room}. Please note that this requires a full room \
					 ID (`!awIh6gGInaS5wLQJwa:example.com`) or a room alias \
					 (`#roomalias:example.com`): {e}"
				);
			},
		};

		debug!("Room specified is a room ID, unbanning room ID");
		self.services.metadata.unban_room(room_id);

		room_id.to_owned()
	} else if room.is_room_alias_id() {
		let room_alias = match RoomAliasId::parse(&room) {
			| Ok(room_alias) => room_alias,
			| Err(e) => {
				return Err!(
					"Failed to parse room ID {room}. Please note that this requires a full room \
					 ID (`!awIh6gGInaS5wLQJwa:example.com`) or a room alias \
					 (`#roomalias:example.com`): {e}"
				);
			},
		};

		debug!(
			"Room specified is not a room ID, attempting to resolve room alias to a room ID \
			 locally, if not using get_alias_helper to fetch room ID remotely"
		);

		let room_id = match self
			.services
			.alias
			.resolve_local_alias(room_alias)
			.await
		{
			| Ok(room_id) => room_id,
			| _ => {
				debug!(
					"We don't have this room alias to a room ID locally, attempting to fetch \
					 room ID over federation"
				);

				match self
					.services
					.alias
					.resolve_alias(room_alias)
					.await
				{
					| Ok((room_id, servers)) => {
						debug!(
							?room_id,
							?servers,
							"Got federation response fetching room ID for room {room}"
						);
						room_id
					},
					| Err(e) => {
						return Err!("Failed to resolve room alias {room} to a room ID: {e}");
					},
				}
			},
		};

		self.services.metadata.unban_room(&room_id);

		room_id
	} else {
		return Err!(
			"Room specified is not a room ID or room alias. Please note that this requires a \
			 full room ID (`!awIh6gGInaS5wLQJwa:example.com`) or a room alias \
			 (`#roomalias:example.com`)",
		);
	};

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
