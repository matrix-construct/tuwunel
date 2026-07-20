mod admin_notice;
mod backup_database;
mod clear_caches;
mod delete_backups;
mod list_backups;
mod list_features;
mod memory_usage;
mod rebuild_relation_index;
mod rebuild_thread_index;
mod reload_config;
mod reload_mods;
#[cfg(unix)]
mod restart;
mod show_config;
mod shutdown;
mod uptime;
mod verify_backup;

use std::{path::PathBuf, sync::Arc};

use clap::Subcommand;
use tuwunel_core::{Result, implement};
use tuwunel_database::Database;

use crate::admin_command_dispatch;

#[admin_command_dispatch]
#[derive(Debug, Subcommand)]
pub(super) enum ServerCommand {
	/// - Time elapsed since startup
	Uptime,

	/// - Show configuration values
	ShowConfig,

	/// - Reload configuration values
	ReloadConfig {
		path: Option<PathBuf>,
	},

	/// - List the features built into the server
	ListFeatures {
		#[arg(short, long)]
		available: bool,

		#[arg(short, long)]
		enabled: bool,

		#[arg(short, long)]
		comma: bool,
	},

	/// - Print database memory usage statistics
	MemoryUsage,

	/// - Clears all of Tuwunel's caches
	ClearCaches,

	/// - Rebuild the typed relation index (relatesto_typed) from all PDUs
	RebuildRelationIndex,

	/// - Rebuild the thread activity index (threadactivityid_rootid) from all
	///   thread roots
	RebuildThreadIndex,

	/// - Performs an online backup of the database (only available for RocksDB
	///   at the moment)
	BackupDatabase,

	/// - List database backups
	ListBackups,

	/// - Verify the files of a database backup are present with their expected
	///   sizes
	VerifyBackup {
		/// Backup ID as listed by list-backups; the most recent backup when
		/// omitted.
		backup_id: Option<u32>,
	},

	/// - Delete database backups, retaining the most recent `keep`
	DeleteBackups {
		/// Number of most-recent backups to retain; zero deletes every backup.
		keep: usize,
	},

	/// - Send a message to the admin room.
	AdminNotice {
		message: Vec<String>,
	},

	/// - Hot-reload the server
	#[clap(alias = "reload")]
	ReloadMods,

	#[cfg(unix)]
	/// - Restart the server
	Restart {
		#[arg(short, long)]
		force: bool,
	},

	/// - Shutdown the server
	Shutdown,
}

/// Run blocking database work off the async runtime.
///
/// Shared by the admin command groups (`server`, `query raw`); the closure
/// receives the `Database` handle on a `spawn_blocking` worker.
#[implement(crate::Context, params = "<'_>")]
pub(crate) async fn blocking_db<F, T>(&self, f: F) -> Result<T>
where
	F: FnOnce(Arc<Database>) -> Result<T> + Send + 'static,
	T: Send + 'static,
{
	let db = Arc::clone(&self.services.db);

	self.services
		.server
		.runtime()
		.spawn_blocking(move || f(db))
		.await?
}
