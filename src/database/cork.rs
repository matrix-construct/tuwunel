//! Write-ahead-log coalescing via a scoped cork guard.
//!
//! Each `Map` insert or remove flushes RocksDB's write-ahead log to the OS
//! immediately after the mutation. "Corking" suppresses that per-write flush:
//! while one or more `Cork` guards are live, WAL records accumulate in the
//! in-memory buffer and reach the OS in a single coalesced batch, trading a
//! syscall per write for one flush per burst.
//!
//! Corking is purely a backend write-buffering optimization. It does not change
//! application logic and has no observable effect on the database API. A write
//! enters the memtable synchronously within the insert or remove call itself,
//! so reads return the new value whether or not a cork is held; the cork
//! governs only when WAL bytes reach the OS (the crash-durability window and
//! the flush syscall count), never what any reader or caller observes.
//!
//! Corks are reference-counted on the `Engine`. `Cork::new` raises the count
//! and `Drop` lowers it, so a guard's scope delimits the coalescing window;
//! nested guards compose, and per-write flushing resumes only when the last one
//! drops.

use std::sync::Arc;

use tuwunel_core::error;

use crate::{Database, Engine};

/// Scoped guard that coalesces write-ahead-log flushes for its lifetime.
///
/// Obtain one from `Database::cork`, `Database::cork_and_flush`, or
/// `Database::cork_and_sync`, hold it across a burst of writes, and drop it to
/// restore per-write flushing. The `flush` and `sync` variants additionally
/// push the buffered WAL out when the guard drops, advancing durability timing
/// only.
#[clippy::has_significant_drop]
pub struct Cork {
	engine: Arc<Engine>,

	/// Flush the WAL buffer to the OS when the guard drops.
	flush: bool,

	/// Sync (fsync) the WAL to disk when the guard drops; implies a flush.
	sync: bool,
}

impl Database {
	/// Open a coalescing window without forcing a flush when it closes.
	///
	/// Per-write WAL flushing is suppressed for the guard's lifetime; the
	/// buffered records are left for the next uncorked write (or RocksDB) to
	/// flush. Use when the burst need not be durable at any particular point.
	#[inline]
	#[must_use]
	pub fn cork(&self) -> Cork { Cork::new(&self.engine, false, false) }

	/// Open a coalescing window that flushes the WAL to the OS on drop.
	///
	/// Behaves like `cork`, but the accumulated WAL is pushed to the OS
	/// (without an fsync) as the guard drops, bounding the buffered window to
	/// the burst.
	#[inline]
	#[must_use]
	pub fn cork_and_flush(&self) -> Cork { Cork::new(&self.engine, true, false) }

	/// Open a coalescing window that syncs the WAL to disk on drop.
	///
	/// Behaves like `cork_and_flush`, but the WAL is fsynced as the guard
	/// drops, so the burst is durable against power loss once the guard has
	/// gone.
	#[inline]
	#[must_use]
	pub fn cork_and_sync(&self) -> Cork { Cork::new(&self.engine, true, true) }
}

impl Cork {
	/// Raise the engine's cork count and capture the on-drop flush policy.
	#[inline]
	pub(super) fn new(engine: &Arc<Engine>, flush: bool, sync: bool) -> Self {
		engine.cork();
		Self { engine: engine.clone(), flush, sync }
	}
}

impl Drop for Cork {
	/// Lower the cork count, then flush and/or sync the WAL per the policy.
	fn drop(&mut self) {
		self.engine.uncork();

		if self.flush {
			self.engine
				.flush()
				.inspect_err(|error| error!(%error, "Failed to flush the write-ahead log."))
				.ok();
		}

		if self.sync {
			self.engine
				.sync()
				.inspect_err(|error| error!(%error, "Failed to sync the write-ahead log."))
				.ok();
		}
	}
}
