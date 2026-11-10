#![cfg(test)]

use std::fs::{create_dir_all, read_dir, remove_dir_all};

use tuwunel::{Args, Runtime, Server, async_exec};
use tuwunel_core::Result;

/// The backup commands round-trip: `backup-database` takes one,
/// `verify-backup` checks its files, and `delete-backups` purges every backup.
#[test]
fn admin_backup_database() -> Result {
	let dir = Args::test_database_path("backup");
	let db = dir.join("db");
	let backup = dir.join("backup");

	create_dir_all(&db)?;
	create_dir_all(&backup)?;

	let args = Args::default_test(&["smoke", "fresh", "cleanup"])
		.with_database_path(&db)
		.with_path_option("database.backup.path", &backup)
		.with_execute("server backup-database")
		.with_execute("server list-backups")
		.with_execute("server verify-backup")
		.with_execute("server delete-backups 0");

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async { async_exec(&server).await });

	drop(runtime);

	// The backup engine keeps one meta file per backup it retains.
	let retained = read_dir(backup.join("meta")).map(Iterator::count);

	remove_dir_all(&dir).ok();
	result?;

	assert_eq!(retained?, 0, "delete-backups leaves no backup behind");

	Ok(())
}
