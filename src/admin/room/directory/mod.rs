mod list;
mod publish;
mod unpublish;

use clap::Subcommand;
use ruma::{OwnedRoomOrAliasId, RoomAliasId, RoomOrAliasId};
use tuwunel_core::{Err, Result};
use tuwunel_service::Services;

use crate::admin_command_dispatch;

#[admin_command_dispatch(handler_prefix = "directory")]
#[derive(Debug, Subcommand)]
pub(crate) enum RoomDirectoryCommand {
	/// - Publish a room to the room directory
	Publish {
		/// The room id or local alias of the room to publish; an alias is
		/// also published as the directory entry's alias
		room: OwnedRoomOrAliasId,

		/// Publish even if the room is unknown to the server, without
		/// recording an alias
		#[arg(long)]
		force: bool,
	},

	/// - Unpublish a room to the room directory
	Unpublish {
		/// The room id or local alias of the room to unpublish
		room: OwnedRoomOrAliasId,
	},

	/// - List rooms that are published
	List {
		page: Option<usize>,
	},
}

/// Parse the alias form of the argument, rejecting a remote alias: these
/// commands resolve without federation.
fn local_alias<'a>(
	services: &Services,
	room: &'a RoomOrAliasId,
) -> Result<Option<&'a RoomAliasId>> {
	match <&RoomAliasId>::try_from(room).ok() {
		| Some(alias) if !services.globals.alias_is_local(alias) =>
			Err!("Alias {alias} is not local to this server; use the room id instead"),
		| alias => Ok(alias),
	}
}
