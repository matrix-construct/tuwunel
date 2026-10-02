mod active_requests;
mod active_requests_for;
mod get_latest_edu_count;
mod parked_rooms;
mod queued_requests;

use clap::Subcommand;
use ruma::{OwnedServerName, OwnedUserId};
use tuwunel_core::{Err, Result};
use tuwunel_service::sending::Destination;

use crate::admin_command_dispatch;

#[admin_command_dispatch(handler_prefix = "sending")]
#[derive(Debug, Subcommand)]
/// All the getters and iterators from src/service/sending/
pub(crate) enum SendingCommand {
	/// - Queries database for all `servercurrentevent_data`
	ActiveRequests,

	/// - Queries database for `servercurrentevent_data` but for a specific
	///   destination
	///
	/// This command takes only *one* format of these arguments:
	///
	/// appservice_id
	/// server_name
	/// user_id AND push_key
	///
	/// See src/service/sending/dest.rs for the definition of the `Destination`
	/// enum
	ActiveRequestsFor {
		#[arg(short, long)]
		appservice_id: Option<String>,
		#[arg(short, long)]
		server_name: Option<OwnedServerName>,
		#[arg(short, long)]
		user_id: Option<OwnedUserId>,
		#[arg(short, long)]
		push_key: Option<String>,
	},

	/// - Queries database for `servernameevent_data` which are the queued up
	///   requests that will eventually be sent
	///
	/// This command takes only *one* format of these arguments:
	///
	/// appservice_id
	/// server_name
	/// user_id AND push_key
	///
	/// See src/service/sending/dest.rs for the definition of the `Destination`
	/// enum
	QueuedRequests {
		#[arg(short, long)]
		appservice_id: Option<String>,
		#[arg(short, long)]
		server_name: Option<OwnedServerName>,
		#[arg(short, long)]
		user_id: Option<OwnedUserId>,
		#[arg(short, long)]
		push_key: Option<String>,
	},

	GetLatestEduCount {
		server_name: OwnedServerName,
	},

	/// - Lists rooms held back from servers that keep rejecting them
	///
	/// A park whose time has passed is retried with the next transaction to
	/// its server; its count resets once the room is delivered.
	ParkedRooms,
}

fn parse_destination(
	appservice_id: Option<String>,
	server_name: Option<OwnedServerName>,
	user_id: Option<OwnedUserId>,
	push_key: Option<String>,
) -> Result<Destination> {
	if appservice_id.is_none() && server_name.is_none() && user_id.is_none() && push_key.is_none()
	{
		return Err!(
			"An appservice ID, server name, or a user ID with push key must be specified via \
			 arguments. See --help for more details.",
		);
	}

	match (appservice_id, server_name, user_id, push_key) {
		| (Some(appservice_id), None, None, None) => {
			if appservice_id.is_empty() {
				return Err!(
					"An appservice ID, server name, or a user ID with push key must be \
					 specified via arguments. See --help for more details.",
				);
			}

			Ok(Destination::Appservice(appservice_id))
		},
		| (None, Some(server_name), None, None) => Ok(Destination::Federation(server_name)),
		| (None, None, Some(user_id), Some(push_key)) => {
			if push_key.is_empty() {
				return Err!(
					"An appservice ID, server name, or a user ID with push key must be \
					 specified via arguments. See --help for more details.",
				);
			}

			Ok(Destination::Push(user_id, push_key))
		},
		| (Some(_), Some(_), Some(_), Some(_)) => Err!(
			"An appservice ID, server name, or a user ID with push key must be specified via \
			 arguments. Not all of them See --help for more details.",
		),
		| _ => Err!(
			"An appservice ID, server name, or a user ID with push key must be specified via \
			 arguments. See --help for more details.",
		),
	}
}
