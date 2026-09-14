#![cfg(test)]

use std::fs::{create_dir_all, remove_dir_all};

use tuwunel::{Args, Runtime, Server, async_exec};
use tuwunel_core::Result;

/// A stop request that reaches the migration entry check ends the process
/// cleanly rather than as a startup failure.
///
/// The distinction is load-bearing for container operators: a failed start is a
/// non-zero exit, which restart policies keyed on failure act upon, while a
/// cancelled one is the server doing as it was asked.
///
/// A fresh database takes the stamp-and-return path, so this covers the entry
/// check and the exit shape, not the per-step gates further down the ladder.
#[test]
fn migration_cancelled_by_shutdown() -> Result {
	let dir = Args::test_database_path("migration-shutdown");
	let db = dir.join("db");

	create_dir_all(&db)?;

	let args = Args::default_test(&["smoke", "fresh", "cleanup"])
		.with_option(format!("database_path=\"{}\"", db.display()));

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;

	// Stands in for a stop request landing while migrations are still running.
	server.server.shutdown()?;

	let result = runtime.block_on(async_exec(&server));

	drop(runtime);
	remove_dir_all(&dir).ok();

	assert!(result.is_ok(), "a cancelled migration must exit cleanly, got {result:?}");

	Ok(())
}
