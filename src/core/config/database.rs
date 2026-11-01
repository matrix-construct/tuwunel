//! Configures online database backups and request workers.
//!
//! Backup settings select storage and retention for RocksDB snapshots. Pool
//! settings control worker affinity, concurrency, and request queue capacity.

use std::path::PathBuf;

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::true_fn;
use crate::utils::sys::available_parallelism;

/// Database backups and worker pool
///
/// Holds two sub-sections: `[global.database.backup]` for online backups,
/// which stay unavailable until `database.backup.path` is set, and
/// `[global.database.pool]` for the threads that serve database requests,
/// sized from the hardware at startup. The database location is
/// `database_path` under `[global]`; engine tuning is in `[global.rocksdb]`.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Default, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.database",
	group = "true",
	ignore = "backup pool"
)]
pub struct Database {
	/// Settings for database backup.
	///
	/// display: section
	#[serde(default)]
	pub backup: Backup,

	/// Settings for database pool.
	///
	/// display: section
	#[serde(default)]
	pub pool: Pool,
}

/// Online database backups
///
/// Copies the RocksDB database into a backup repository while the server
/// keeps running. Backups are unavailable until `path` names a directory
/// the server can write to, and even then run only on request, with
/// `!admin server backup-database`. Both keys apply on reload.
///
/// Tips:
/// - `keep` defaults to 1, so each backup replaces the previous one; raise
///   it to retain more.
/// - Backups hold the database only. Media stored under `database_path` or
///   in storage providers needs its own copy.
/// - For scheduled backups, add `"server backup-database"` to
///   `admin.signal_execute` and send the server `SIGUSR2` from a timer.
/// - To restore, stop the server and start it with `--restore-backup`; it
///   replaces the database at `database_path` before opening it. See
///   <https://tuwunel.chat/backups.html>.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.database.backup",
	group = "true"
)]
pub struct Backup {
	/// Selects a writable directory for online database backups.
	///
	/// Backups use the RocksDB Backup engine API. See
	/// https://tuwunel.chat/maintenance.html#backups for setup and
	/// maintenance.
	///
	/// reloadable: yes
	/// example: "/opt/tuwunel-db-backups"
	/// legacy: database_backup_path
	pub path: Option<PathBuf>,

	/// Sets how many online RocksDB backups to retain.
	///
	/// When `database.backup.path` is configured, the oldest backups are
	/// deleted to enforce this limit. The value must be at least 1;
	/// `backup-database` returns an error at 0 or below.
	///
	/// reloadable: yes
	/// default: 1
	/// legacy: database_backups_to_keep
	#[serde(default = "default_keep")]
	pub keep: i16,
}

/// Database worker pool
///
/// The threads that execute database reads and writes, and the queues that
/// feed them. Sizes are derived when the database opens, from the CPU count
/// and, where it can be detected, the queue layout of the storage device
/// under `database_path`. Changes need a restart.
///
/// Tips:
/// - Under a cgroup task (pids) limit, as in many containers, startup warns
///   when the pool may exceed it; raise the limit or lower `max_workers`.
/// - `[global.runtime.stream]` scales its concurrency from this pool.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.database.pool",
	group = "true"
)]
pub struct Pool {
	/// Enables database pool affinity support.
	///
	/// On supporting systems, detected block device queue topology
	/// determines `database.pool.workers` automatically and optimizes the
	/// request pool for the hardware.
	///
	/// default: true
	/// legacy: db_pool_affinity
	#[serde(default = "true_fn")]
	pub affinity: bool,

	/// Sets the number of database frontend pool worker threads.
	///
	/// Match the system I/O capabilities, such as queue depth or
	/// simultaneous requests in flight. The default is four times the
	/// available CPU thread count, clamped from 32 through 1024. This
	/// setting applies only when `database.pool.affinity` is disabled or
	/// unavailable; otherwise the worker count is determined automatically.
	///
	/// default: varies by system
	/// legacy: db_pool_workers
	#[serde(default = "default_workers")]
	pub workers: usize,

	/// Limits each automatically detected database worker group.
	///
	/// When `database.pool.affinity` is enabled and available, this value is
	/// multiplied by the number of CPU cores sharing a device queue because
	/// workers can run on any of those cores. The limit prevents performance
	/// degradation when thread pooling cannot scale to the full capacity of
	/// high-end hardware.
	///
	/// default: 32
	/// legacy: db_pool_workers_limit
	#[serde(default = "default_workers_limit")]
	pub workers_limit: usize,

	/// Limits the total number of workers across all database worker groups.
	///
	/// Worker counts are reduced when their sum exceeds this limit, but each
	/// populated group retains at least one worker, making the populated
	/// group count the effective floor. The default generally constrains
	/// larger systems with 16 or more cores. Thread pooling cannot reach
	/// full hardware capacity, but this limit can be raised on huge systems
	/// when groups are too small and scheduling overhead is not the
	/// bottleneck.
	///
	/// default: 2048
	/// legacy: db_pool_max_workers
	#[serde(default = "default_max_workers")]
	pub max_workers: usize,

	/// Sets the capacity multiplier for database frontend pool request
	/// queues.
	///
	/// Each queue capacity is this value multiplied by the pool worker
	/// count. Tokio tasks submitting requests yield when the queue is full,
	/// providing flow control that avoids buffer bloat. A value that is too
	/// low can inhibit throughput.
	///
	/// default: 4
	/// legacy: db_pool_queue_mult
	#[serde(default = "default_queue_mult")]
	pub queue_mult: usize,
}

impl Default for Backup {
	fn default() -> Self { Self { path: None, keep: default_keep() } }
}

impl Default for Pool {
	fn default() -> Self {
		Self {
			affinity: true_fn(),
			workers: default_workers(),
			workers_limit: default_workers_limit(),
			max_workers: default_max_workers(),
			queue_mult: default_queue_mult(),
		}
	}
}

fn default_keep() -> i16 { 1 }

fn default_workers() -> usize {
	available_parallelism()
		.saturating_mul(4)
		.clamp(32, 1024)
}

fn default_workers_limit() -> usize { 32 }

fn default_max_workers() -> usize { 2048 }

fn default_queue_mult() -> usize { 4 }
