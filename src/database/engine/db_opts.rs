use std::{cmp, convert::TryFrom};

use rocksdb::{Cache, DBRecoveryMode, Env, LogLevel, Options, statistics::StatsLevel};
use tuwunel_core::{Config, Result, utils};

use super::{cf_opts::cache_size_f64, logger::handle as handle_log};

/// Create database-wide options suitable for opening the database. This also
/// sets our default column options in case of opening a column with the same
/// resulting value. Note that we require special per-column options on some
/// columns, therefor columns should only be opened after passing this result
/// through cf_options().
pub(crate) fn db_options(config: &Config, env: &Env, row_cache: &Cache) -> Result<Options> {
	const DEFAULT_STATS_LEVEL: StatsLevel = if cfg!(debug_assertions) {
		StatsLevel::ExceptDetailedTimers
	} else {
		StatsLevel::DisableAll
	};

	let mut opts = Options::default();

	// Logging
	set_logging_defaults(&mut opts, config);

	// Processing
	opts.set_max_background_jobs(num_threads::<i32>(config)?);
	opts.set_max_subcompactions(num_threads::<u32>(config)?);
	opts.set_avoid_unnecessary_blocking_io(true);
	opts.set_max_file_opening_threads(0);

	// IO
	opts.set_manual_wal_flush(true);
	opts.set_atomic_flush(config.rocksdb_atomic_flush);
	opts.set_enable_pipelined_write(!config.rocksdb_atomic_flush);
	if config.rocksdb_direct_io {
		opts.set_use_direct_reads(true);
		opts.set_use_direct_io_for_flush_and_compaction(true);
	}
	if config.rocksdb_optimize_for_spinning_disks {
		opts.set_skip_stats_update_on_db_open(true);
		//opts.set_max_file_opening_threads(threads.try_into().unwrap());
	} else {
		opts.set_max_file_opening_threads(num_threads(config)?);
		opts.set_compaction_readahead_size(1024 * 512);
	}

	// Blocks
	opts.set_row_cache(row_cache);
	opts.set_db_write_buffer_size(cache_size_f64(
		config,
		config.db_write_buffer_capacity_mb,
		1_048_576,
	));

	// Files
	opts.set_table_cache_num_shard_bits(7);
	opts.set_wal_size_limit_mb(1024);
	opts.set_max_total_wal_size(1024 * 1024 * 512);
	opts.set_writable_file_max_buffer_size(1024 * 1024 * 2);

	// Misc
	opts.set_disable_auto_compactions(!config.rocksdb_compaction);
	opts.create_missing_column_families(true);
	opts.create_if_missing(true);

	opts.set_statistics_level(match config.rocksdb_stats_level {
		| 0 => StatsLevel::DisableAll,
		| 1 => DEFAULT_STATS_LEVEL,
		| 2 => StatsLevel::ExceptHistogramOrTimers,
		| 3 => StatsLevel::ExceptTimers,
		| 4 => StatsLevel::ExceptDetailedTimers,
		| 5 => StatsLevel::ExceptTimeForMutex,
		| 6_u8..=u8::MAX => StatsLevel::All,
	});

	opts.set_report_bg_io_stats(match config.rocksdb_stats_level {
		| 0..=1 => false,
		| 2_u8..=u8::MAX => true,
	});

	// Default: https://github.com/facebook/rocksdb/wiki/WAL-Recovery-Modes#ktoleratecorruptedtailrecords
	//
	// Unclean shutdowns of a Matrix homeserver are likely to be fine when
	// recovered in this manner as it's likely any lost information will be
	// restored via federation.
	opts.set_wal_recovery_mode(match config.rocksdb_recovery_mode {
		| 0 => DBRecoveryMode::AbsoluteConsistency,
		| 1 => DBRecoveryMode::TolerateCorruptedTailRecords,
		| 2 => DBRecoveryMode::PointInTime,
		| 3 => DBRecoveryMode::SkipAnyCorruptedRecord,
		| 4_u8..=u8::MAX => unimplemented!(),
	});

	// <https://github.com/facebook/rocksdb/wiki/Track-WAL-in-MANIFEST>
	// "We recommend to set track_and_verify_wals_in_manifest to true for
	// production, it has been enabled in production for the entire database cluster
	// serving the social graph for all Meta apps."
	opts.set_track_and_verify_wals_in_manifest(true);

	opts.set_paranoid_checks(config.rocksdb_paranoid_file_checks);

	opts.set_env(env);

	Ok(opts)
}

fn set_logging_defaults(opts: &mut Options, config: &Config) {
	let rocksdb_log_level = match config.rocksdb_log_level.as_ref() {
		| "debug" => LogLevel::Debug,
		| "info" => LogLevel::Info,
		| "warn" => LogLevel::Warn,
		| "fatal" => LogLevel::Fatal,
		| _ => LogLevel::Error,
	};

	opts.set_log_level(rocksdb_log_level);
	opts.set_max_log_file_size(config.rocksdb_max_log_file_size);
	opts.set_log_file_time_to_roll(config.rocksdb_log_time_to_roll);
	opts.set_keep_log_file_num(config.rocksdb_max_log_files);
	opts.set_stats_dump_period_sec(0);

	if config.rocksdb_log_stderr {
		opts.set_stderr_logger(rocksdb_log_level, "rocksdb");
	} else {
		opts.set_callback_logger(rocksdb_log_level, handle_log);
	}
}

fn num_threads<T: TryFrom<usize>>(config: &Config) -> Result<T> {
	const MIN_PARALLELISM: usize = 2;

	let requested = if config.rocksdb_parallelism_threads != 0 {
		config.rocksdb_parallelism_threads
	} else {
		utils::available_parallelism()
	};

	utils::math::try_into::<T, usize>(cmp::max(MIN_PARALLELISM, requested))
}
