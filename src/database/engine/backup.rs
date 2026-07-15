use std::path::Path;

use rocksdb::backup::{BackupEngine, BackupEngineInfo, BackupEngineOptions, RestoreOptions};
use tuwunel_core::{
	Config, Err, Result, err, error, implement, info, itertools::Itertools,
	utils::time::rfc2822_from_seconds, warn,
};

use super::Engine;
use crate::{Context, util::map_err};

#[implement(Engine)]
#[tracing::instrument(skip(self))]
pub fn backup(&self) -> Result {
	let mut engine = backup_engine(&self.ctx)?;
	let config = &self.ctx.server.config;
	if config.database_backups_to_keep > 0 {
		let flush = !self.is_read_only();
		engine
			.create_new_backup_flush(&self.db, flush)
			.map_err(map_err)?;

		let engine_info = engine.get_backup_info();
		let info = &engine_info
			.last()
			.expect("backup engine info is not empty");
		info!(
			"Created database backup #{} using {} bytes in {} files",
			info.backup_id, info.size, info.num_files,
		);
	}

	if config.database_backups_to_keep >= 0 {
		let keep = u32::try_from(config.database_backups_to_keep)?;
		if let Err(e) = engine.purge_old_backups(keep.try_into()?) {
			error!("Failed to purge old backup: {e:?}");
		}
	}

	if config.database_backups_to_keep == 0 {
		warn!("Configuration item `database_backups_to_keep` is set to 0.");
	}

	Ok(())
}

#[implement(Engine)]
pub fn backup_list(&self) -> Result<impl Iterator<Item = String> + Send> {
	let info = backup_engine(&self.ctx)?.get_backup_info();

	if info.is_empty() {
		return Err!("No backups found.");
	}

	let list = info.into_iter().map(|info| {
		format!(
			"#{} {}: {} bytes, {} files",
			info.backup_id,
			rfc2822_from_seconds(info.timestamp),
			info.size,
			info.num_files,
		)
	});

	Ok(list)
}

#[implement(Engine)]
pub fn backup_count(&self) -> Result<usize> {
	let info = backup_engine(&self.ctx)?.get_backup_info();

	Ok(info.len())
}

#[implement(Engine)]
pub fn backup_verify(&self, backup_id: u32) -> Result<u32> {
	let engine = backup_engine(&self.ctx)?;
	let backup = find_backup(&engine, backup_id)?;

	engine
		.verify_backup(backup.backup_id)
		.map_err(map_err)?;

	Ok(backup.backup_id)
}

/// Restore a backup over the configured database path, replacing the database
/// files found there. Must complete prior to opening the database.
pub(crate) fn restore(ctx: &Context, backup_id: u32) -> Result {
	let mut engine = backup_engine(ctx)?;
	let backup = find_backup(&engine, backup_id)?;
	let path = &ctx.server.config.database_path;

	warn!(
		backup_id = backup.backup_id,
		timestamp = %rfc2822_from_seconds(backup.timestamp),
		size = backup.size,
		num_files = backup.num_files,
		?path,
		"Restoring database backup"
	);

	engine
		.restore_from_backup(path, path, &RestoreOptions::default(), backup.backup_id)
		.map_err(map_err)?;

	info!(backup_id = backup.backup_id, "Restored database backup");

	Ok(())
}

/// Backup ID 0 selects the most recent backup.
fn find_backup(engine: &BackupEngine, backup_id: u32) -> Result<BackupEngineInfo> {
	let mut backups = engine.get_backup_info();

	if backups.is_empty() {
		return Err!("No backups found.");
	}

	let found = match backup_id {
		| 0 => backups.pop(),
		| id => backups
			.iter()
			.position(|info| info.backup_id == id)
			.map(|pos| backups.swap_remove(pos)),
	};

	found.ok_or_else(|| {
		let available = backups
			.iter()
			.map(|info| info.backup_id)
			.join(", ");

		err!("Backup #{backup_id} not found; available: {available}")
	})
}

fn backup_engine(ctx: &Context) -> Result<BackupEngine> {
	let path = backup_path(&ctx.server.config)?;
	let options = BackupEngineOptions::new(path).map_err(map_err)?;

	BackupEngine::open(&options, &*ctx.env.lock()?).map_err(map_err)
}

fn backup_path(config: &Config) -> Result<&Path> {
	config
		.database_backup_path
		.as_deref()
		.filter(|path| !path.as_os_str().is_empty())
		.ok_or_else(|| err!(Config("database_backup_path", "Configure path to enable backups")))
}
