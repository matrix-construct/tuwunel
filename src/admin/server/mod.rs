mod admin_notice;
mod backup_database;
mod checkpoint_database;
mod clear_caches;
mod delete_backups;
mod list_backups;
mod list_features;
mod memory_usage;
mod regenerate_config;
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

	/// - Reload configuration values, layering an optional extra file over the
	///   ones the server started with
	ReloadConfig {
		path: Option<PathBuf>,
	},

	#[command(about = "Regenerate configuration into a server-local file.")]
	RegenerateConfig {
		#[arg(help = "Absolute destination path on this server.")]
		path: PathBuf,

		#[arg(
			long,
			help = "Replace an existing destination after saving a backup."
		)]
		force: bool,

		#[arg(
			long,
			help = "Materialize values currently supplied by the environment."
		)]
		include_env: bool,

		#[arg(
			long,
			help = "Comment out keys that are not recognized by this binary."
		)]
		strip_unknown: bool,
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

	/// - Performs an online backup of the database (only available for RocksDB
	///   at the moment)
	BackupDatabase,

	/// - Create a physical RocksDB checkpoint of the database or one map
	CheckpointDatabase {
		/// Map to export instead of checkpointing the complete database.
		#[arg(long, alias = "column")]
		map: Option<String>,

		/// Destination directory. Defaults inside the current database
		/// directory.
		#[arg(long)]
		path: Option<PathBuf>,

		/// Write-ahead log size threshold for flushing a database checkpoint.
		#[arg(long, num_args = 0..=1, default_value_t = 0, default_missing_value = "0")]
		log_size: u64,
	},

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
