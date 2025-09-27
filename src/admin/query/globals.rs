use clap::Subcommand;
use ruma::OwnedServerName;
use tuwunel_core::Result;

use crate::Context;

#[derive(Debug, Subcommand)]
/// All the getters and iterators from src/database/key_value/globals.rs
pub(crate) enum GlobalsCommand {
	DatabaseVersion,

	CurrentCount,

	/// - This returns an empty `Ok(BTreeMap<..>)` when there are no keys found
	///   for the server.
	SigningKeysFor {
		origin: OwnedServerName,
	},
}

/// All the getters and iterators from src/database/key_value/globals.rs
pub(super) async fn process(subcommand: GlobalsCommand, context: &Context<'_>) -> Result<String> {
	let services = context.services;

	match subcommand {
		| GlobalsCommand::DatabaseVersion => {
			let timer = tokio::time::Instant::now();
			let results = services.globals.db.database_version().await;
			let query_time = timer.elapsed();

			Ok(format!("Query completed in {query_time:?}:\n\n```rs\n{results:#?}\n```"))
		},
		| GlobalsCommand::CurrentCount => {
			let timer = tokio::time::Instant::now();
			let results = services.globals.current_count();
			let query_time = timer.elapsed();

			Ok(format!("Query completed in {query_time:?}:\n\n```rs\n{results:#?}\n```"))
		},
		| GlobalsCommand::SigningKeysFor { origin } => {
			let timer = tokio::time::Instant::now();
			let results = services
				.server_keys
				.verify_keys_for(&origin)
				.await;
			let query_time = timer.elapsed();

			Ok(format!("Query completed in {query_time:?}:\n\n```rs\n{results:#?}\n```"))
		},
	}
}
