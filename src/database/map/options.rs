use std::sync::Arc;

use rocksdb::{ReadOptions, ReadTier, WriteOptions};

use crate::Engine;

/// Builds iterator options restricted to block-cache reads.
///
/// The probe neither fills cache nor falls through to storage.
#[inline]
pub(crate) fn cache_iter_options_default(engine: &Arc<Engine>) -> ReadOptions {
	let mut options = iter_options_default(engine);
	options.set_read_tier(ReadTier::BlockCache);
	options.fill_cache(false);
	options
}

/// Builds the default options for a map iterator.
///
/// Iterator cleanup may purge obsolete files in the background.
#[inline]
pub(crate) fn iter_options_default(engine: &Arc<Engine>) -> ReadOptions {
	let mut options = read_options_default(engine);
	options.set_background_purge_on_iterator_cleanup(true);
	options
}

/// Builds the options for a whole-column integrity walk.
///
/// Checksums are verified whatever the engine setting, and the walk leaves the
/// block cache as it found it.
#[inline]
pub(crate) fn scan_options_default(engine: &Arc<Engine>) -> ReadOptions {
	let mut options = iter_options_default(engine);

	options.set_verify_checksums(true);
	options.fill_cache(false);
	options
}

/// Builds point-read options restricted to block cache.
///
/// The read neither fills cache nor falls through to storage.
#[inline]
pub(crate) fn cache_read_options_default(engine: &Arc<Engine>) -> ReadOptions {
	let mut options = read_options_default(engine);
	options.set_read_tier(ReadTier::BlockCache);
	options.fill_cache(false);
	options
}

/// Builds the base options shared by map reads and iterators.
///
/// Total-order seek is enabled. Checksum verification follows the engine
/// setting.
#[inline]
pub(crate) fn read_options_default(engine: &Arc<Engine>) -> ReadOptions {
	let mut options = ReadOptions::default();
	options.set_total_order_seek(true);

	if !engine.checksums {
		options.set_verify_checksums(false);
	}

	options
}

/// Builds the default options for a map write.
///
/// The current engine configuration requires no map-specific overrides.
#[inline]
pub(crate) fn write_options_default(_engine: &Arc<Engine>) -> WriteOptions {
	WriteOptions::default()
}
