//! `flush` raw query: force a RocksDB LSM-tree memtable flush.
//!
//! Calls [`tuwunel_database::Engine::sort`], flushing RocksDB's memtables to
//! on-disk SST files. An LSM-tree flush, not a libc `fflush(3)` or `fsync(2)`,
//! and not the write-ahead-log `flush`/`sync` engine methods.

use tokio::time::Instant;
use tuwunel_core::Result;

use crate::admin_command;

/// Flush the RocksDB memtables to SST files: an LSM-tree flush, not
/// `fflush(3)` or `fsync(2)`.
#[admin_command]
pub(super) async fn raw_flush(&self) -> Result {
	let timer = Instant::now();

	self.blocking_db(|db| db.engine.sort()).await?;

	let elapsed = timer.elapsed();

	write!(self, "Memtables flushed to SST files in {elapsed:?}.").await
}
