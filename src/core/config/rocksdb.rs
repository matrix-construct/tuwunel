//! Configures the RocksDB engine and its resource limits.
//!
//! Settings cover memory, compression, logging, and database recovery.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::{parallelism_scaled_f64, true_fn};
use crate::utils::bytes::deserialize_bytesize_usize;

/// RocksDB engine configuration.
///
/// Omitted settings preserve the server's engine defaults.
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.rocksdb",
	group = "true"
)]
pub struct Rocksdb {
	/// Sets the memory available to database read caches as a float in megabytes.
	///
	/// Increasing this may improve performance when memory is available; like
	/// individual LRU caches, the default scales with CPU core count to 128.0 + (64.0
	/// * CPU core count).
	///
	/// default: varies by system
	/// legacy: db_cache_capacity_mb
	#[serde(default = "default_cache_capacity_mb")]
	pub cache_capacity_mb: f64,

	/// Sets the memory available to database write caches as a float in megabytes.
	///
	/// Increasing this may improve performance when memory is available; like
	/// individual LRU caches, the default scales with CPU core count to 48.0 + (4.0 *
	/// CPU core count).
	///
	/// default: varies by system
	/// legacy: db_write_buffer_capacity_mb
	#[serde(default = "default_write_buffer_capacity_mb")]
	pub write_buffer_capacity_mb: f64,

	/// Sets the log level for RocksDB LOG files in the database directory.
	///
	/// This differs from the application log level; tuwunel still reports RocksDB
	/// errors through tracing or panics on severe errors for safety.
	///
	/// default: "error"
	/// legacy: rocksdb_log_level
	#[serde(default = "default_log_level")]
	pub log_level: String,

	/// Routes RocksDB log messages to standard error.
	///
	/// `rocksdb.log_level` still filters the emitted records. When disabled,
	/// RocksDB uses the application's callback logger instead.
	/// legacy: rocksdb_log_stderr
	#[serde(default)]
	pub log_stderr: bool,

	/// Sets the maximum RocksDB LOG file size before rotation.
	///
	/// Accepts an integer byte count or a string with SI/IEC suffix such as "4 MiB".
	///
	/// default: 4194304
	/// legacy: rocksdb_max_log_file_size
	#[serde(
		default = "default_max_log_file_size",
		deserialize_with = "deserialize_bytesize_usize"
	)]
	pub max_log_file_size: usize,

	/// Time in seconds before RocksDB will forcibly rotate logs.
	///
	/// default: 0
	/// legacy: rocksdb_log_time_to_roll
	#[serde(default = "default_log_time_to_roll")]
	pub log_time_to_roll: usize,

	/// Uses RocksDB tunings tailored to spinning disks (HDDs).
	///
	/// Leave this disabled on NVMe or SSD storage. When enabled, RocksDB skips
	/// compaction readahead and parallel file-open threads at startup. This does not
	/// affect Direct IO; see `rocksdb.direct_io`.
	///
	/// legacy: rocksdb_optimize_for_spinning_disks
	#[serde(default)]
	pub optimize_for_spinning_disks: bool,

	/// Enables direct-io to increase database performance via unbuffered I/O.
	///
	/// For more details about direct I/O and RockDB, see:
	/// https://github.com/facebook/rocksdb/wiki/Direct-IO
	///
	/// Set this option to false if the database resides on a filesystem which
	/// does not support direct-io like FUSE, or any form of complex filesystem
	/// setup such as possibly ZFS.
	/// legacy: rocksdb_direct_io
	#[serde(default = "true_fn")]
	pub direct_io: bool,

	/// Sets the number of RocksDB threads for cleanup, sync, flush, and compaction.
	///
	/// The stored value 0 selects the available logical thread count, with a minimum
	/// of two.
	///
	/// default: 0
	/// legacy: rocksdb_parallelism_threads
	#[serde(default = "default_parallelism_threads")]
	pub parallelism_threads: usize,

	/// Sets the maximum number of RocksDB LOG files retained.
	///
	/// This must be at least 1, never 0. The default is 3 because these logs are
	/// mainly useful for troubleshooting or debugging RocksDB bugs.
	///
	/// default: 3
	/// legacy: rocksdb_max_log_files
	#[serde(default = "default_max_log_files")]
	pub max_log_files: usize,

	/// Sets the RocksDB database compression algorithm.
	///
	/// Available options are "zstd", "bz2", "lz4", or "none", which disables
	/// compression. ZSTD balances speed, storage, IO amplification, and CPU usage;
	/// LZ4 offers more performance and lower CPU usage with less compression and more
	/// storage use. See https://github.com/facebook/rocksdb/wiki/Compression for
	/// details.
	///
	/// default: "zstd"
	/// legacy: rocksdb_compression_algo
	#[serde(default = "default_compression_algo")]
	pub compression_algo: String,

	/// Level of compression the specified compression algorithm for RocksDB to
	/// use.
	///
	/// Default is 32767, which is internally read by RocksDB as the default
	/// magic number and translated to the library's default compression level
	/// as they all differ. See their `kDefaultCompressionLevel`.
	///
	/// Note when using the default value we may override it with a setting
	/// tailored specifically tuwunel.
	///
	/// default: 32767
	/// legacy: rocksdb_compression_level
	#[serde(default = "default_compression_level")]
	pub compression_level: i32,

	/// Sets the compression level for the bottommost RocksDB data.
	///
	/// The default 32767 is RocksDB's `kDefaultCompressionLevel`, translated to the
	/// selected library's default level; tuwunel may override it with a tailored
	/// setting. Bottommost data is generally old and less frequently accessed, so
	/// higher compression may be useful; research the chosen algorithm.
	///
	/// default: 32767
	/// legacy: rocksdb_bottommost_compression_level
	#[serde(default = "default_bottommost_compression_level")]
	pub bottommost_compression_level: i32,

	/// Whether to enable RocksDB's "bottommost_compression".
	///
	/// At the expense of more CPU usage, this will further compress the
	/// database to reduce more storage. It is recommended to use ZSTD
	/// compression with this for best compression results. This may be useful
	/// if you're trying to reduce storage usage from the database.
	///
	/// See https://github.com/facebook/rocksdb/wiki/Compression for more details.
	/// legacy: rocksdb_bottommost_compression
	#[serde(default = "true_fn")]
	pub bottommost_compression: bool,

	/// Sets the database recovery mode for RocksDB WAL corruption.
	///
	/// When corruption prevents startup, use mode 2 (PointInTime) to recover to the
	/// last good state, seconds or minutes before the crash. Clients may need
	/// "clear-cache & reload" after this rollback. After recovery succeeds, reset the mode
	/// to its default and restart. In some cases the corruption error persists for at
	/// least 30 minutes of operation in PointInTime mode.
	///
	/// Only as a last resort if PointInTime fails, try mode 3
	/// (SkipAnyCorruptedRecord), which may leave the server inconsistent. Default
	/// mode 1 (TolerateCorruptedTailRecords) drops only the last entry if corrupted
	/// during shutdown, with an extraordinarily low risk of desynchronizing clients.
	/// Mode 0 (AbsoluteConsistency) disables all silent rollback. See
	/// https://github.com/facebook/rocksdb/wiki/WAL-Recovery-Modes for mode details
	/// and https://tuwunel.chat/troubleshooting.html#database-corruption for recovery
	/// guidance.
	///
	/// default: 1
	/// legacy: rocksdb_recovery_mode
	#[serde(default = "default_recovery_mode")]
	pub recovery_mode: u8,

	/// Enables paranoid SST file checks.
	///
	/// Additional safety checks may improve database consistency at a performance
	/// cost; see the RocksDB documentation for details:
	/// https://github.com/facebook/rocksdb/wiki/Online-Verification#columnfamilyoptionsparanoid_file_checks
	///
	/// legacy: rocksdb_paranoid_file_checks
	#[serde(default)]
	pub paranoid_file_checks: bool,

	/// Enables runtime checksum verification in RocksDB.
	///
	/// Checksums are enabled by default and usually hardware accelerated with low
	/// overhead. Older or slower platforms may gain performance by disabling them.
	///
	/// default: true
	/// legacy: rocksdb_checksums
	#[serde(default = "true_fn")]
	pub checksums: bool,

	/// Enables RocksDB atomic flush mode.
	///
	/// This option is not intended for users and may be removed or ignored in future
	/// versions. It may improve database integrity at the cost of performance.
	///
	/// legacy: rocksdb_atomic_flush
	#[serde(default)]
	pub atomic_flush: bool,

	/// Enables database repair for RocksDB SST corruption.
	///
	/// Use this when the server reports corruption while running or panics,
	/// especially for errors containing SST after startup; if startup fails, try
	/// recovery mode first. Back up the database directory before running repair.
	/// Disable repair mode and restart the server after repair finishes. See
	/// https://tuwunel.chat/troubleshooting.html#database-corruption for recovery
	/// details.
	///
	/// legacy: rocksdb_repair
	#[serde(default)]
	pub repair: bool,

	/// Opens RocksDB in read-only mode.
	///
	/// Writes are rejected and missing column families cannot be created. This
	/// mode is disabled by default.
	/// legacy: rocksdb_read_only
	#[serde(default)]
	pub read_only: bool,

	/// Opens RocksDB as a secondary follower of a primary instance.
	///
	/// Writes are rejected while the primary's latest WAL can be replayed into
	/// this instance's view. Missing column families cannot be created.
	/// legacy: rocksdb_secondary
	#[serde(default)]
	pub secondary: bool,

	/// Enables idle CPU priority for the compaction thread.
	///
	/// This is disabled by default to prevent compaction from falling too far behind
	/// on busy systems.
	///
	/// legacy: rocksdb_compaction_prio_idle
	#[serde(default)]
	pub compaction_prio_idle: bool,

	/// Enables idle IO priority for the compaction thread.
	///
	/// This is enabled by default and usually advisable to prevent unexpected server
	/// lag.
	///
	/// legacy: rocksdb_compaction_ioprio_idle
	#[serde(default = "true_fn")]
	pub compaction_ioprio_idle: bool,

	/// Enables RocksDB compaction.
	///
	/// You should never need to disable this; if troubleshooting or a bug appears to
	/// require it, contact the tuwunel Matrix room with details. Disabling compaction
	/// causes explosive database growth, gradually poor performance, excessive disk
	/// reads and writes, and slower shutdowns and startups.
	///
	/// legacy: rocksdb_compaction
	#[serde(default = "true_fn")]
	pub compaction: bool,

	/// Sets the level of database statistics collection from 0 to 6.
	///
	/// Some admin commands that display database statistics require this setting;
	/// higher levels may affect performance. Level 0 disables statistics, level 1
	/// disables them in release mode and is the default, levels 2 to 3 have no
	/// performance impact, levels 3 to 5 may affect performance, and level 6 collects
	/// all statistics.
	///
	/// default: 1
	/// legacy: rocksdb_stats_level
	#[serde(default = "default_stats_level")]
	pub stats_level: u8,

	/// Ignores the list of dropped columns set by developers.
	///
	/// This should be set to true when knowingly moving between versions in
	/// ways which are not recommended or otherwise forbidden, or for
	/// diagnostic and development purposes; requiring preservation across such
	/// movements.
	///
	/// The developer's list of dropped columns is meant to safely reduce space
	/// by erasing data no longer in use. If this is set to true that storage
	/// will not be reclaimed as intended.
	///
	/// default: false
	/// legacy: rocksdb_never_drop_columns
	#[serde(default)]
	pub never_drop_columns: bool,

	/// Allows RocksDB to preallocate WAL logs.
	///
	/// Normally, RocksDB allocates certain types of files by calling
	/// fallocate, writing the file contents, then truncating the logs to the
	/// proper size. This causes pathological disk space usage on btrfs due to
	/// how it interacts with its Copy-on-Write implementation. On ZFS,
	/// fallocate(2) for preallocation is unsupported and returns EOPNOTSUPP;
	/// only `FALLOC_FL_PUNCH_HOLE` and `FALLOC_FL_ZERO_RANGE` are implemented.
	///
	/// Set this to false if you run the server on btrfs or ZFS, and do not
	/// touch it otherwise.
	///
	/// default: true
	/// legacy: rocksdb_allow_fallocate
	#[serde(default = "true_fn")]
	pub allow_fallocate: bool,
}

impl Default for Rocksdb {
	fn default() -> Self {
		Self {
			cache_capacity_mb: default_cache_capacity_mb(),
			write_buffer_capacity_mb: default_write_buffer_capacity_mb(),
			log_level: default_log_level(),
			log_stderr: Default::default(),
			max_log_file_size: default_max_log_file_size(),
			log_time_to_roll: default_log_time_to_roll(),
			optimize_for_spinning_disks: Default::default(),
			direct_io: true_fn(),
			parallelism_threads: default_parallelism_threads(),
			max_log_files: default_max_log_files(),
			compression_algo: default_compression_algo(),
			compression_level: default_compression_level(),
			bottommost_compression_level: default_bottommost_compression_level(),
			bottommost_compression: true_fn(),
			recovery_mode: default_recovery_mode(),
			paranoid_file_checks: Default::default(),
			checksums: true_fn(),
			atomic_flush: Default::default(),
			repair: Default::default(),
			read_only: Default::default(),
			secondary: Default::default(),
			compaction_prio_idle: Default::default(),
			compaction_ioprio_idle: true_fn(),
			compaction: true_fn(),
			stats_level: default_stats_level(),
			never_drop_columns: Default::default(),
			allow_fallocate: true_fn(),
		}
	}
}

fn default_cache_capacity_mb() -> f64 { 128.0 + parallelism_scaled_f64(64.0) }

fn default_write_buffer_capacity_mb() -> f64 { 48.0 + parallelism_scaled_f64(4.0) }

fn default_log_level() -> String { "error".to_owned() }

fn default_max_log_file_size() -> usize { 4 * 1024 * 1024 }

fn default_log_time_to_roll() -> usize { 0 }

fn default_parallelism_threads() -> usize { 0 }

fn default_max_log_files() -> usize { 3 }

fn default_compression_algo() -> String {
	cfg!(feature = "zstd_compression")
		.then_some("zstd")
		.unwrap_or("none")
		.to_owned()
}

/// Default RocksDB compression level is 32767, which is internally read by
/// RocksDB as the default magic number and translated to the library's default
/// compression level as they all differ.
///
/// See their `kDefaultCompressionLevel`.
#[expect(clippy::doc_markdown)]
fn default_compression_level() -> i32 { 32767 }

/// Default RocksDB compression level is 32767, which is internally read by
/// RocksDB as the default magic number and translated to the library's default
/// compression level as they all differ.
///
/// See their `kDefaultCompressionLevel`.
#[expect(clippy::doc_markdown)]
fn default_bottommost_compression_level() -> i32 { 32767 }

fn default_recovery_mode() -> u8 { 1 }

fn default_stats_level() -> u8 { 1 }
