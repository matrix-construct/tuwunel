mod drop_connections;
mod list_connections;
mod show_connection;

use clap::Subcommand;
use ruma::{OwnedDeviceId, OwnedUserId};
use tuwunel_core::Result;

use crate::admin_command_dispatch;

#[admin_command_dispatch]
#[derive(Debug, Subcommand)]
/// Query sync service state
pub(crate) enum SyncCommand {
	/// List sliding-sync connections.
	ListConnections,

	/// Show details of sliding sync connection by ID.
	ShowConnection {
		user_id: OwnedUserId,
		device_id: Option<OwnedDeviceId>,
		conn_id: Option<String>,
	},

	/// Drop connections for a user, device, or all.
	///
	/// Each affected client starts over with an initial sync, even one with a
	/// request in flight.
	DropConnections {
		user_id: Option<OwnedUserId>,
		device_id: Option<OwnedDeviceId>,
		conn_id: Option<String>,
	},
}
