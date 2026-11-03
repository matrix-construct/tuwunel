#![cfg(test)]

use std::fs::{create_dir_all, remove_dir_all};

use tuwunel::{Args, Runtime, Server, async_exec};
use tuwunel_core::Result;

/// Retaining no backups leaves `backup-database` nothing to create, so it must
/// report the failure rather than report success having done nothing.
#[test]
fn admin_backup_disabled() -> Result {
	let dir = Args::test_database_path("backup-disabled");
	let db = dir.join("db");
	let backup = dir.join("backup");

	create_dir_all(&db)?;
	create_dir_all(&backup)?;

	let args = Args::default_test(&["smoke", "fresh", "cleanup"])
		.with_database_path(&db)
		.with_path_option("database.backup.path", &backup)
		.with_option("database.backup.keep=0")
		.with_execute("server backup-database");

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async { async_exec(&server).await });

	drop(runtime);
	remove_dir_all(&dir).ok();

	// Under the console feature the command output is reported separately, leaving
	// the error itself without the config option named in it.
	result.expect_err("backup-database must fail when no backup can be created");

	Ok(())
}
