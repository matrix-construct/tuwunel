mod list;
mod publish;
mod unpublish;

use clap::Subcommand;
use ruma::OwnedRoomOrAliasId;
use tuwunel_core::Result;

use crate::admin_command_dispatch;

#[admin_command_dispatch(handler_prefix = "directory")]
#[derive(Debug, Subcommand)]
pub(crate) enum RoomDirectoryCommand {
	/// - Publish a room to the room directory
	Publish {
		/// The room id or alias of the room to publish; an alias is also
		/// published as the directory entry's alias
		room: OwnedRoomOrAliasId,

		/// Publish the room id only, without recording an alias
		#[arg(long)]
		force: bool,
	},

	/// - Unpublish a room to the room directory
	Unpublish {
		/// The room id or alias of the room to unpublish
		room: OwnedRoomOrAliasId,
	},

	/// - List rooms that are published
	List {
		page: Option<usize>,
	},
}
