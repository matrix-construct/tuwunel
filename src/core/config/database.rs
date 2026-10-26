//! Configures online database backups and request workers.
//!
//! Backup settings select storage and retention for RocksDB snapshots. Pool
//! settings control worker affinity, concurrency, and request queue capacity.

use std::path::PathBuf;

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::true_fn;
use crate::utils::sys::available_parallelism;

/// Groups database backup and request pool settings.
///
/// Backups control snapshot storage and retention. The request pool controls
/// how database work is distributed across workers and hardware queues.
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

/// Configures RocksDB online backup storage and retention.
///
/// Backups require a writable destination path. Retention limits how many
/// snapshots remain after a backup completes.
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

/// Configures the worker pool serving database requests.
///
/// Affinity can derive worker groups from block device queue topology. Worker
/// limits and queue capacity constrain concurrency and scheduling overhead.
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
