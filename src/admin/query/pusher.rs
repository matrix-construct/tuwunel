use clap::Subcommand;
use ruma::OwnedUserId;
use tuwunel_core::Result;
use tuwunel_macros::{admin_command, admin_command_dispatch};

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
}

#[admin_command]
pub(super) async fn get_pushers(&self, user_id: OwnedUserId) -> Result {
	let timer = tokio::time::Instant::now();
	let results = self.services.pusher.get_pushers(&user_id).await;
	let query_time = timer.elapsed();

	self.write_string(format!("Query completed in {query_time:?}:\n\n```rs\n{results:#?}```"))
		.await
}

#[admin_command]
pub(super) async fn remove_pusher(&self, user_id: OwnedUserId, pushkey: String) -> Result {
	let exists = self
		.services
		.pusher
		.get_pusher(&user_id, &pushkey)
		.await
		.is_ok();

	self.services
		.pusher
		.delete_pusher(&user_id, &pushkey)
		.await;

	let message = if exists {
		"Pusher deleted."
	} else {
		"Pusher was not found but deletion was still attempted."
	};

	self.write_str(message).await
}
