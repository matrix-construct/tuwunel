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
}

#[admin_command]
pub(super) async fn get_pushers(&self, user_id: OwnedUserId) -> Result {
	let timer = tokio::time::Instant::now();
	let results = self.services.pusher.get_pushers(&user_id).await;
	let query_time = timer.elapsed();

	self.write_string(format!("Query completed in {query_time:?}:\n\n```rs\n{results:#?}```"))
		.await
}
