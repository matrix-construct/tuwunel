#![cfg(test)]

use std::{
	env::temp_dir,
	fs::{create_dir_all, read_dir, remove_dir_all},
	process::id as process_id,
};

use tuwunel::{Args, Runtime, Server, async_exec};
use tuwunel_core::Result;

/// The backup commands round-trip: `backup-database` takes one,
/// `verify-backup` checks its files, and `delete-backups` purges every backup.
#[test]
fn admin_backup_database() -> Result {
	let dir = temp_dir().join(format!("tuwunel-backup-{}", process_id()));
	let db = dir.join("db");
	let backup = dir.join("backup");

	create_dir_all(&db)?;
	create_dir_all(&backup)?;

	let mut args = Args::default_test(&["smoke", "fresh", "cleanup"]);

	args.option.extend([
		format!("database_path=\"{}\"", db.display()),
		format!("database_backup_path=\"{}\"", backup.display()),
	]);

	args.execute.extend([
		"server backup-database".to_owned(),
		"server list-backups".to_owned(),
		"server verify-backup".to_owned(),
		"server delete-backups 0".to_owned(),
	]);

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
