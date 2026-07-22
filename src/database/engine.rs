//! RocksDB engine: database-wide operations and shared resources.
//!
//! `Engine` owns the opened RocksDB instance together with the worker pool, the
//! shared open-time context, and the flags fixed at open (read-only, secondary,
//! checksums). Per-column-family reads and writes go through `Map`; the methods
//! here act on the database as a whole: WAL flush and sync, memtable flush,
//! manual compaction and primary catch-up, property queries, and the cork
//! counter that coalesces WAL writes (see the `cork` module).

mod backup;
mod cf_opts;
pub(crate) mod context;
mod db_opts;
pub(crate) mod descriptor;
mod events;
mod files;
mod logger;
mod memory_usage;
mod open;
mod repair;
#[cfg(test)]
mod tests;

use std::{
	collections::BTreeMap,
	ffi::CStr,
	sync::{
		Arc, OnceLock, Weak,
		atomic::{AtomicU32, Ordering},
	},
};

use rocksdb::{
	AsColumnFamilyRef, BoundColumnFamily, DBCommon, DBWithThreadMode, MultiThreaded,
	WaitForCompactOptions, WriteOptions,
};
use tuwunel_core::{Err, Result, debug, implement, info, warn};

use crate::{
	Context, Map,
	pool::Pool,
	util::{map_err, result},
};

pub(crate) type CfIndex = BTreeMap<u32, Weak<Map>>;

/// Handle to the opened RocksDB database and its shared resources.
///
/// One `Engine` exists per database, shared behind an `Arc` by every `Map`.
pub struct Engine {
	/// The opened RocksDB instance.
	pub(crate) db: Db,

	/// Thread pool offloading uncached, blocking database requests from the
	/// tokio workers.
	pub(crate) pool: Arc<Pool>,

	/// Resources constructed before the database is opened and outliving it
	/// (block caches, environment, column descriptors).
	pub(crate) ctx: Arc<Context>,

	/// Database was opened read-only; writes are rejected.
	pub(super) read_only: bool,

	/// Database was opened as a secondary follower of a primary instance.
	pub(super) secondary: bool,

	/// Verify block checksums on read.
	pub(crate) checksums: bool,

	/// Shared write options for atomic batch commits.
	pub(crate) write_options: WriteOptions,

	/// Resolves catalog column ids for post-commit watcher notification.
	/// Runtime migration column families are intentionally absent.
	cf_index: OnceLock<CfIndex>,

	/// Live cork count; nonzero suppresses the per-write WAL flush.
	corks: AtomicU32,
}

/// Backing RocksDB type: multi-threaded column-family access, no transactions.
pub(crate) type Db = DBWithThreadMode<MultiThreaded>;

impl Engine {
	/// Block until outstanding background compactions finish.
	///
	/// Waits without a timeout and does not flush first; aborts the wait if
	/// compaction has been paused.
	#[tracing::instrument(
		level = "info",
		skip_all,
		fields(
			sequence = ?self.current_sequence(),
		),
	)]
	pub fn wait_compactions_blocking(&self) -> Result {
		let mut opts = WaitForCompactOptions::default();
		opts.set_abort_on_pause(true);
		opts.set_flush(false);
		opts.set_timeout(0);

		self.db.wait_for_compact(&opts).map_err(map_err)
	}

	/// Flush the memtables to SST files (a RocksDB LSM-tree flush).
	///
	/// Forces buffered writes out of memory into the on-disk LSM tree. An LSM
	/// flush, not a libc `fflush(3)` or `fsync(2)`, and distinct from the
	/// `flush` and `sync` methods here, which act on the write-ahead log.
	#[tracing::instrument(
		level = "info",
		skip_all,
		fields(
			sequence = ?self.current_sequence(),
		),
	)]
	pub fn sort(&self) -> Result {
		let flushoptions = rocksdb::FlushOptions::default();
		result(DBCommon::flush_opt(&self.db, &flushoptions))
	}

	/// Catch a secondary instance up to the primary's latest writes.
	///
	/// Replays the primary's newly appended WAL into this instance's view;
	/// meaningful only when the database was opened as a secondary.
	#[tracing::instrument(
		level = "debug",
		skip_all,
		fields(
			sequence = ?self.current_sequence(),
		),
	)]
	pub fn update(&self) -> Result {
		self.db
			.try_catch_up_with_primary()
			.map_err(map_err)
	}

	/// Flush the write-ahead log and fsync it to disk.
	///
	/// Once this returns the buffered writes survive power loss. Heavier than
	/// `flush`, which stops at the OS page cache.
	#[tracing::instrument(level = "info", skip_all)]
	pub fn sync(&self) -> Result { result(DBCommon::flush_wal(&self.db, true)) }

	/// Flush the buffered write-ahead log to the OS without an fsync.
	///
	/// Pushes WAL bytes to the page cache (durable against process crash, not
	/// power loss). This is the per-write flush that corking suppresses.
	#[tracing::instrument(level = "debug", skip_all)]
	pub fn flush(&self) -> Result { result(DBCommon::flush_wal(&self.db, false)) }

	/// Increment the cork count, suppressing the per-write WAL flush.
	#[inline]
	pub(crate) fn cork(&self) { self.corks.fetch_add(1, Ordering::Relaxed); }

	/// Decrement the cork count; the per-write flush resumes at zero.
	#[inline]
	pub(crate) fn uncork(&self) { self.corks.fetch_sub(1, Ordering::Relaxed); }

	/// Whether any cork is currently held.
	///
	/// When true, `Map` insert and remove skip their post-write WAL flush so
	/// the records coalesce into one batch. Corking is purely a backend
	/// write-buffering signal: it never changes application logic or any
	/// observable database API behavior, because a write lands in the memtable
	/// synchronously and reads back regardless of WAL flush state. See the
	/// `cork` module.
	#[inline]
	pub fn corked(&self) -> bool { self.corks.load(Ordering::Relaxed) > 0 }

	/// Query for database property by null-terminated name which is expected to
	/// have a result with an integer representation. This is intended for
	/// low-overhead programmatic use.
	pub(crate) fn property_integer(
		&self,
		cf: &impl AsColumnFamilyRef,
		name: &CStr,
	) -> Result<u64> {
		result(self.db.property_int_value_cf(cf, name))
			.and_then(|val| val.map_or_else(|| Err!("Property {name:?} not found."), Ok))
	}

	/// Query for database property by name receiving the result in a string.
	pub(crate) fn property(&self, cf: &impl AsColumnFamilyRef, name: &str) -> Result<String> {
		result(self.db.property_value_cf(cf, name))
			.and_then(|val| val.map_or_else(|| Err!("Property {name:?} not found."), Ok))
	}

	/// Look up a column-family handle by name.
	///
	/// Panics if the family was not described before the database was opened.
	pub(crate) fn cf(&self, name: &str) -> Arc<BoundColumnFamily<'_>> {
		self.db
			.cf_handle(name)
			.expect("column must be described prior to database open")
	}

	/// Whether a column family with this name exists.
	#[inline]
	#[must_use]
	pub fn has_cf(&self, name: &str) -> bool { self.db.cf_handle(name).is_some() }

	/// The latest RocksDB sequence number, a monotonic counter of writes.
	#[inline]
	#[must_use]
	#[tracing::instrument(
		name = "sequence",
		level = "debug",
		skip_all,
		fields(sequence)
	)]
	pub fn current_sequence(&self) -> u64 {
		let sequence = self.db.latest_sequence_number();

		#[cfg(debug_assertions)]
		tracing::Span::current().record("sequence", sequence);

		sequence
	}

	/// Whether writes are rejected: true for a read-only or secondary open.
	#[inline]
	#[must_use]
	pub fn is_read_only(&self) -> bool { self.secondary || self.read_only }

	/// Whether the database was opened as a secondary follower of a primary.
	#[inline]
	#[must_use]
	pub fn is_secondary(&self) -> bool { self.secondary }
}

#[implement(Engine)]
pub(crate) fn set_cf_index(&self, index: CfIndex) {
	self.cf_index
		.set(index)
		.expect("cf_index initialized twice");
}

#[implement(Engine)]
#[inline]
pub(crate) fn map_by_cf_id(&self, cf_id: u32) -> Option<Arc<Map>> {
	self.cf_index
		.get()
		.expect("cf_index initialized before writes")
		.get(&cf_id)
		.and_then(Weak::upgrade)
}

impl Drop for Engine {
	#[cold]
	fn drop(&mut self) {
		const BLOCKING: bool = true;

		debug!("Waiting for background tasks to finish...");
		self.db.cancel_all_background_work(BLOCKING);

		info!(
			sequence = %self.current_sequence(),
			"Closing database..."
		);
	}
}
