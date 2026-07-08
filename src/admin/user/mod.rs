mod add_email;
mod create_user;
mod deactivate;
mod deactivate_all;
mod del_email;
mod delete_device;
mod delete_room_tag;
mod erasure;
mod force_demote;
mod force_join_all_local_users;
mod force_join_list_of_local_users;
mod force_join_room;
mod force_leave_room;
mod force_promote;
mod get_room_tags;
mod last_active;
mod list_joined_rooms;
mod list_users;
mod make_user_admin;
mod put_room_tag;
mod redact_event;
mod reject_invites;
mod reset_password;
mod unerase;

use clap::Subcommand;
use futures::FutureExt;
use ruma::{OwnedDeviceId, OwnedEventId, OwnedRoomId, OwnedRoomOrAliasId, OwnedUserId, UserId};
use tuwunel_core::Result;
use tuwunel_service::{Services, users::DeactivationReason};

use crate::admin_command_dispatch;

const AUTO_GEN_PASSWORD_LENGTH: usize = 25;
const BULK_JOIN_REASON: &str = "Bulk force joining this room as initiated by the server admin.";

#[admin_command_dispatch]
#[derive(Debug, Subcommand)]
pub(super) enum UserCommand {
	/// - Create a new user
	#[clap(alias = "create")]
	CreateUser {
		/// Username of the new user
		username: String,
		/// Password of the new user, if unspecified one is generated
		password: Option<String>,
	},

	/// - Reset user password
	ResetPassword {
		/// Username of the user for whom the password should be reset
		username: String,
		/// New password for the user, if unspecified one is generated
		password: Option<String>,
	},

	/// - Bind an email address to a local user without verification
	AddEmail {
		/// Local user to bind the email address to
		username: String,
		/// Email address to bind
		address: String,
	},

	/// - Remove an email address binding from a local user
	DelEmail {
		/// Local user to remove the email address from
		username: String,
		/// Email address to remove
		address: String,
	},

	/// - Deactivate a user
	///
	/// User will be removed from all rooms by default.
	/// Use --no-leave-rooms to not leave all rooms by default.
	Deactivate {
		#[arg(short, long)]
		no_leave_rooms: bool,
		user_id: String,
	},

	/// - Deactivate a list of users
	///
	/// Recommended to use in conjunction with list-local-users.
	///
	/// Users will be removed from joined rooms by default.
	///
	/// Can be overridden with --no-leave-rooms.
	///
	/// Removing a mass amount of users from a room may cause a significant
	/// amount of leave events. The time to leave rooms may depend significantly
	/// on joined rooms and servers.
	///
	/// This command needs a newline separated list of users provided in a
	/// Markdown code block below the command.
	DeactivateAll {
		#[arg(short, long)]
		/// Does not leave any rooms the user is in on deactivation
		no_leave_rooms: bool,
		#[arg(short, long)]
		/// Also deactivate admin accounts and will assume leave all rooms too
		force: bool,
	},

	/// - Show the MSC4025 erasure state of a local user
	Erasure {
		user_id: String,
	},

	/// - Clear the MSC4025 erasure marker of a local user, restoring the
	///   unredacted view of their events
	Unerase {
		user_id: String,
	},

	/// - Deletes a user's device.
	DeleteDevice {
		user_id: OwnedUserId,
		device_id: OwnedDeviceId,
	},

	/// - List local users by recent activity.
	LastActive {
		#[arg(short, long)]
		limit: Option<usize>,
	},

	/// - List local users in the database
	#[clap(alias = "list")]
	ListUsers,

	/// - Lists all the rooms (local and remote) that the specified user is
	///   joined in
	ListJoinedRooms {
		user_id: String,
	},

	/// - Manually join a local user to a room.
	ForceJoinRoom {
		user_id: String,
		room: OwnedRoomOrAliasId,
	},

	/// - Manually leave a local user from a room.
	ForceLeaveRoom {
		user_id: String,
		room_id: OwnedRoomOrAliasId,
	},

	/// - Reject all pending invites for a local user.
	RejectInvites {
		user_id: String,

		/// Optional reason attached to each rejection.
		#[arg(long)]
		reason: Option<String>,
	},

	/// - Forces the specified user to drop their power levels to the room
	///   default, if their permissions allow and the auth check permits
	ForceDemote {
		user_id: String,
		room_id: OwnedRoomOrAliasId,
	},

	/// - Force promote
	ForcePromote {
		user_id: String,
		room_id: OwnedRoomOrAliasId,
	},

	/// - Grant server-admin privileges to a user.
	MakeUserAdmin {
		user_id: String,
	},

	/// - Puts a room tag for the specified user and room ID.
	///
	/// This is primarily useful if you'd like to set your admin room
	/// to the special "System Alerts" section in Element as a way to
	/// permanently see your admin room without it being buried away in your
	/// favourites or rooms. To do this, you would pass your user, your admin
	/// room's internal ID, and the tag name `m.server_notice`.
	PutRoomTag {
		user_id: String,
		room_id: OwnedRoomId,
		tag: String,
	},

	/// - Deletes the room tag for the specified user and room ID
	DeleteRoomTag {
		user_id: String,
		room_id: OwnedRoomId,
		tag: String,
	},

	/// - Gets all the room tags for the specified user and room ID
	GetRoomTags {
		user_id: String,
		room_id: OwnedRoomId,
	},

	/// - Attempts to forcefully redact the specified event ID from the sender
	///   user
	///
	/// This is only valid for local users
	RedactEvent {
		event_id: OwnedEventId,
	},

	/// - Force joins a specified list of local users to join the specified
	///   room.
	///
	/// Specify a codeblock of usernames.
	///
	/// Requires the `--yes-i-want-to-do-this` flag.
	ForceJoinListOfLocalUsers {
		room: OwnedRoomOrAliasId,

		#[arg(long)]
		yes_i_want_to_do_this: bool,
	},

	/// - Force joins all local users to the specified room.
	///
	/// Requires the `--yes-i-want-to-do-this` flag.
	ForceJoinAllLocalUsers {
		room: OwnedRoomOrAliasId,

		#[arg(long)]
		yes_i_want_to_do_this: bool,
	},
}

async fn deactivate_user(services: &Services, user_id: &UserId, no_leave_rooms: bool) -> Result {
	if !no_leave_rooms {
		services
			.deactivate
			.full_deactivate(user_id, false, DeactivationReason::Admin)
			.boxed()
			.await?;
	} else {
		services
			.users
			.deactivate_account(user_id, DeactivationReason::Admin)
			.await?;
	}

	Ok(())
}
