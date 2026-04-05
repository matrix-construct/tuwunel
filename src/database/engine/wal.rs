use rocksdb::{WriteBatch, WriteOptions};
use tuwunel_core::{Error, Result, implement};

use super::Engine;
use crate::util::map_err;

/// Apply a raw WriteBatch (from the primary's WAL stream) to this database.
///
/// Used by the secondary replication worker to replay incoming batches.
#[implement(Engine)]
pub fn write_raw_batch(&self, data: &[u8]) -> Result {
	let batch = WriteBatch::from_data(data);
	let opts = WriteOptions::default();

	self.db.write_opt(&batch, &opts).map_err(map_err)
}

/// Return a WAL iterator starting at `since`.
///
/// Yields batches whose sequence number is >= `since`. If `since` is older
/// than the oldest retained WAL segment, this returns `Err` — call
/// `is_wal_gap_error` on the result to distinguish this case from other
/// errors.
#[implement(Engine)]
#[inline]
pub fn wal_updates_since(&self, since: u64) -> Result<rocksdb::DBWALIterator> {
	self.db.get_updates_since(since).map_err(map_err)
}

/// Returns `true` if `err` indicates the requested WAL sequence is older
/// than any retained segment on this instance.
///
/// The primary uses this to return HTTP 410 rather than 500.
pub fn is_wal_gap_error(err: &Error) -> bool {
	let msg = err.to_string().to_lowercase();
	msg.contains("too old")
		|| msg.contains("older than")
		|| msg.contains("sequence not")
		|| msg.contains("data loss")
		|| msg.contains("not available")
}

/// Extract the operation count from a raw WriteBatch byte slice.
///
/// RocksDB `WriteBatch` layout: `[8 bytes sequence][4 bytes count][records…]`.
/// The count at bytes 8–11 is the number of operations in the batch, which
/// equals how many sequence numbers the batch consumes. Returns 0 if the
/// slice is too short to contain the count.
#[inline]
pub(crate) fn batch_count_from_bytes(data: &[u8]) -> u64 {
	if data.len() < 12 {
		return 0;
	}

	//TODO: XXX
	u64::from(u32::from_le_bytes(data[8..12].try_into().expect("4 bytes")))
}
