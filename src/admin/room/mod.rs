mod alias;
mod delete;
mod directory;
mod exists;
mod info;
mod list;
mod list_extremities;
mod moderation;
mod prune_empty;
mod prune_extremities;
mod purge_user;

use clap::Subcommand;
use ruma::{OwnedRoomId, OwnedRoomOrAliasId};
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

	/// - Prune empty rooms
	PruneEmpty {
		#[arg(short, long)]
		force: bool,
	},

	/// - List a room's forward extremities with a total
	ListExtremities {
		room_id: OwnedRoomOrAliasId,
	},

	/// - Scored prune of a room's forward extremities down to a target
	PruneExtremities {
		room_id: OwnedRoomOrAliasId,

		/// Target frontier size (default: forward_extremities_max, min 1)
		target: Option<usize>,

		/// Show the plan without writing
		#[arg(long)]
		dry_run: bool,
	},

	/// - Delete every room a user is joined to
	///
	/// Useful for cleaning up after spam invitations or a faulty appservice
	/// registration. With --regex the argument is a pattern matched against
	/// every joined member of each room, so a whole namespace
	/// (e.g. `@bot_[A-Za-z0-9]+:example\.com`) can be cleared at once.
	PurgeUser {
		/// A user ID, or (with --regex) a pattern matched against the joined
		/// members of every room
		user_id: String,

		/// Interpret user_id as a regular expression
		#[arg(long)]
		regex: bool,

		/// Only delete rooms where the matched user is the only joined member
		#[arg(long)]
		sole_member: bool,

		/// List the rooms that would be deleted without deleting them
		#[arg(long)]
		dry_run: bool,
	},
}
