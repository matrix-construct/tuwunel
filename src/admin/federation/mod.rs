mod disable_room;
mod enable_room;
mod fetch_support_well_known;
mod incoming_federation;
mod remote_user_in_rooms;

use clap::Subcommand;
use ruma::{OwnedRoomId, OwnedServerName, OwnedUserId};
use tuwunel_core::Result;

use crate::admin_command_dispatch;

#[admin_command_dispatch]
#[derive(Debug, Subcommand)]
pub(super) enum FederationCommand {
	/// - List all rooms we are currently handling an incoming pdu from
	IncomingFederation,

	/// - Disables incoming federation handling for a room.
	///
	/// The room's `m.federate` property is unaffected; it is fixed at room
	/// creation.
	DisableRoom {
		room_id: OwnedRoomId,
	},

	/// - Enables incoming federation handling for a room again.
	///
	/// The room's `m.federate` property is unaffected; it is fixed at room
	/// creation.
	EnableRoom {
		room_id: OwnedRoomId,
	},

	/// - Fetch `/.well-known/matrix/support` from the specified server
	///
	/// Despite the name, this is not a federation endpoint and does not go
	/// through the federation / server resolution process as per-spec this is
	/// supposed to be served at the server_name.
	///
	/// Respecting homeservers put this file here for listing administration,
	/// moderation, and security inquiries. This command provides a way to
	/// easily fetch that information.
	FetchSupportWellKnown {
		server_name: OwnedServerName,
	},

	/// - Lists all the rooms we share/track with the specified *remote* user
	RemoteUserInRooms {
		user_id: OwnedUserId,
	},
}
