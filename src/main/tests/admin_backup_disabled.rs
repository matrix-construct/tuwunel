#![cfg(test)]

use std::{
	env::temp_dir,
	fs::{create_dir_all, remove_dir_all},
	process::id as process_id,
};

use tuwunel::{Args, Runtime, Server, async_exec};
use tuwunel_core::Result;

/// Retaining no backups leaves `backup-database` nothing to create, so it must
/// report the failure rather than report success having done nothing.
#[test]
fn admin_backup_disabled() -> Result {
	let dir = temp_dir().join(format!("tuwunel-backup-disabled-{}", process_id()));
	let db = dir.join("db");
	let backup = dir.join("backup");

	create_dir_all(&db)?;
	create_dir_all(&backup)?;

	let mut args = Args::default_test(&["smoke", "fresh", "cleanup"]);

	args.option.extend([
		format!("database_path=\"{}\"", db.display()),
		format!("database_backup_path=\"{}\"", backup.display()),
		"database_backups_to_keep=0".to_owned(),
	]);

	args.execute.push("server backup-database".into());

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
