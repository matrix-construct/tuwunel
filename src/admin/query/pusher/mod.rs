mod drop_queue;
mod get_pushers;
mod list_queue;
mod remove_pusher;
mod retry_queue;

use clap::Subcommand;
use ruma::OwnedUserId;
use tuwunel_core::Result;

use crate::admin_command_dispatch;

#[admin_command_dispatch]
#[derive(Debug, Subcommand)]
pub(crate) enum PusherCommand {
	/// - Returns all the pushers for the user.
	GetPushers {
		/// Full user ID
		user_id: OwnedUserId,
	},

	/// - Manually delete a pusher for a user.
	RemovePusher {
		/// Full user ID
		user_id: OwnedUserId,

		/// Pushkey
		pushkey: String,
	},

	/// Drop pending pushes; already claimed work may continue or requeue.
	DropQueue {
		/// Full user ID.
		user_id: OwnedUserId,

		/// Pushkey.
		pushkey: String,
	},

	/// List queued, active, and suppressed push events.
	ListQueue {
		/// Full user ID.
		user_id: OwnedUserId,

		/// Pushkey.
		pushkey: String,
	},

	/// Retry durable pushes through the sender, preserving backoff and suppression.
	RetryQueue {
		/// Full user ID.
		user_id: OwnedUserId,

		/// Pushkey.
		pushkey: String,
	},
}
