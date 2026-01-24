use std::fmt::Write;

use clap::Subcommand;
use futures::StreamExt;
use ruma::{OwnedRoomAliasId, OwnedRoomId};
use tuwunel_core::{Err, Result, err};
use tuwunel_macros::{admin_command, admin_command_dispatch};
use tuwunel_service::Services;

use crate::Context;

#[admin_command_dispatch(handler_prefix = "alias")]
#[derive(Debug, Subcommand)]
pub(crate) enum RoomAliasCommand {
	/// - Make an alias point to a room.
	Set {
		#[arg(short, long)]
		/// Set the alias even if a room is already using it
		force: bool,

		/// The room id to set the alias on
		room_id: OwnedRoomId,

		/// The alias localpart to use (`alias`, not `#alias:servername.tld`)
		room_alias_localpart: String,
	},

	/// - Remove a local alias
	Remove {
		/// The alias localpart to remove (`alias`, not `#alias:servername.tld`)
		room_alias_localpart: String,
	},

	/// - Show which room is using an alias
	Which {
		/// The alias localpart to look up (`alias`, not
		/// `#alias:servername.tld`)
		room_alias_localpart: String,
	},

	/// - List aliases currently being used
	List {
		/// If set, only list the aliases for this room
		room_id: Option<OwnedRoomId>,
	},
}

fn parse_alias_from_localpart(
	services: &Services,
	room_alias_localpart: &String,
) -> Result<OwnedRoomAliasId> {
	let room_alias_str = format!("#{}:{}", room_alias_localpart, services.globals.server_name());

	Ok(OwnedRoomAliasId::try_from(room_alias_str)?)
}

#[admin_command]
pub(super) async fn alias_set(
	&self,
	force: bool,
	room_id: OwnedRoomId,
	room_alias_localpart: String,
) -> Result {
	let room_alias = parse_alias_from_localpart(self.services, &room_alias_localpart)?;

	match self
		.services
		.alias
		.resolve_local_alias(&room_alias)
		.await
	{
		| Ok(id) => {
			if !force {
				return Err!(
					"Refusing to overwrite in use alias for {id}, use -f or --force to overwrite"
				);
			}

			self.services
				.alias
				.set_alias(&room_alias, &room_id)
				.map_err(|err| err!("Failed to remove alias: {err}"))?;

			self.write_str(&format!("Successfully overwrote alias (formerly {id})"))
				.await
		},
		| _ => {
			self.services
				.alias
				.set_alias(&room_alias, &room_id)
				.map_err(|err| err!("Failed to remove alias: {err}"))?;

			self.write_str("Successfully set alias").await
		},
	}
}

#[admin_command]
pub(super) async fn alias_remove(&self, room_alias_localpart: String) -> Result {
	let room_alias = parse_alias_from_localpart(self.services, &room_alias_localpart)?;

	let id = self
		.services
		.alias
		.resolve_local_alias(&room_alias)
		.await
		.map_err(|_| err!("Alias isn't in use."))?;

	self.services
		.alias
		.remove_alias(&room_alias)
		.await
		.map_err(|err| err!("Failed to remove alias: {err}"))?;

	self.write_str(&format!("Removed alias from {id}"))
		.await
}

#[admin_command]
pub(super) async fn alias_which(&self, room_alias_localpart: String) -> Result {
	let room_alias = parse_alias_from_localpart(self.services, &room_alias_localpart)?;

	let id = self
		.services
		.alias
		.resolve_local_alias(&room_alias)
		.await
		.map_err(|_| err!("Alias isn't in use."))?;

	self.write_str(&format!("Alias resolves to {id}"))
		.await
}

#[admin_command]
pub(super) async fn alias_list(&self, room_id: Option<OwnedRoomId>) -> Result {
	match room_id {
		| Some(room_id) => list_aliases_for_room(self, room_id).await,
		| None => list_all_aliases(self).await,
	}
}

async fn list_aliases_for_room(context: &Context<'_>, room_id: OwnedRoomId) -> Result {
	let aliases: Vec<OwnedRoomAliasId> = context
		.services
		.alias
		.local_aliases_for_room(&room_id)
		.map(Into::into)
		.collect()
		.await;

	let mut plain_list = String::new();

	for alias in aliases {
		writeln!(plain_list, "- {alias}")?;
	}

	let plain = format!("Aliases for {room_id}:\n{plain_list}");
	context.write_str(&plain).await
}

async fn list_all_aliases(context: &Context<'_>) -> Result {
	let aliases = context
		.services
		.alias
		.all_local_aliases()
		.map(|(room_id, localpart)| (room_id.to_owned(), localpart.to_owned()))
		.collect::<Vec<_>>()
		.await;

	let server_name = context.services.globals.server_name();

	let mut plain_list = String::new();
	for (room_id, alias_id) in aliases {
		writeln!(plain_list, "- `{room_id}` -> #{alias_id}:{server_name}")?;
	}

	let plain = format!("Aliases:\n{plain_list}");
	context.write_str(&plain).await
}
