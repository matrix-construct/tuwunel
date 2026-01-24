mod alias;
mod commands;
mod directory;
mod info;
mod moderation;

use clap::Subcommand;
use ruma::OwnedRoomId;
use tuwunel_core::Result;

use self::{
	alias::RoomAliasCommand, directory::RoomDirectoryCommand, info::RoomInfoCommand,
	moderation::RoomModerationCommand,
};
use crate::admin_command_dispatch;

#[admin_command_dispatch(handler_prefix = "room")]
#[derive(Debug, Subcommand)]
pub(super) enum RoomCommand {
	/// - List all rooms the server knows about
	List {
		page: Option<usize>,

		/// Excludes rooms that we have federation disabled with
		#[arg(long)]
		exclude_disabled: bool,

		/// Excludes rooms that we have banned
		#[arg(long)]
		exclude_banned: bool,

		#[arg(long)]
		/// Whether to only output room IDs without supplementary room
		/// information
		no_details: bool,
	},

	#[command(subcommand)]
	/// - View information about a room we know about
	Info(RoomInfoCommand),

	#[command(subcommand)]
	/// - Manage moderation of remote or local rooms
	Moderation(RoomModerationCommand),

	#[command(subcommand)]
	/// - Manage room aliases
	Alias(RoomAliasCommand),

	#[command(subcommand)]
	/// - Manage the room directory
	Directory(RoomDirectoryCommand),

	/// - Check if we know about a room
	Exists {
		room_id: OwnedRoomId,
	},

	/// - Delete room
	Delete {
		room_id: OwnedRoomId,

		#[arg(short, long)]
		force: bool,
	},
}
