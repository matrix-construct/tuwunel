use rocksdb::{BottommostLevelCompaction, CompactOptions};
use tuwunel_core::{Err, Result, implement};

use crate::keyval::KeyBuf;

/// Configures a manual compaction for a map.
///
/// A range can limit selected keys, while level selection controls compaction
/// placement. Completion and exclusivity flags determine how aggressively
/// RocksDB runs the operation.
#[derive(Clone, Debug, Default)]
pub struct Options {
	/// Bounds the key range selected for compaction.
	///
	/// A missing lower or upper bound leaves that side of the range unbounded.
	pub range: (Option<KeyBuf>, Option<KeyBuf>),

	/// Describes the supported manual-compaction level modes.
	///
	/// `(None, None)` lets RocksDB choose placement, and `(None, Some(target))`
	/// compacts all levels into `target`. `(Some(level), None)` validates
	/// `level` but leaves normal placement unchanged; two explicit levels are
	/// unsupported.
	pub level: (Option<usize>, Option<usize>),

	/// Controls whether bottommost data is compacted fully.
	///
	/// When disabled, RocksDB avoids recompacting bottommost files created by
	/// this compaction. Enabling this option forces bottommost compaction.
	pub exhaustive: bool,

	/// Controls whether manual compaction runs exclusively.
	///
	/// When enabled, RocksDB waits for ongoing compactions and pauses automatic
	/// compaction until this operation finishes.
	pub exclusive: bool,
}

/// Compacts this map synchronously with the supplied options.
///
/// The key range and supported target placement are forwarded to RocksDB
/// manual compaction. Unsupported level combinations and invalid target levels
/// are returned to the caller.
#[implement(super::Map)]
#[tracing::instrument(
	name = "compact",
	level = "info"
	skip(self),
	fields(%self),
)]
pub fn compact_blocking(&self, opts: Options) -> Result {
	let mut co = CompactOptions::default();
	co.set_exclusive_manual_compaction(opts.exclusive);
	co.set_bottommost_level_compaction(match opts.exhaustive {
		| true => BottommostLevelCompaction::Force,
		| false => BottommostLevelCompaction::ForceOptimized,
	});

	match opts.level {
		| (None, None) => {
			co.set_change_level(true);
			co.set_target_level(-1);
		},
		| (None, Some(level)) => {
			co.set_change_level(true);
			co.set_target_level(level.try_into()?);
		},
		| (Some(level), None) => {
			co.set_change_level(false);
			co.set_target_level(level.try_into()?);
		},
		| (Some(_), Some(_)) => return Err!("compacting between specific levels not supported"),
	}

	self.engine
		.db
		.compact_range_cf_opt(&self.cf(), opts.range.0, opts.range.1, &co);

	Ok(())
}
