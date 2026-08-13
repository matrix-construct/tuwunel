use std::sync::Arc;

use futures::{Stream, TryStreamExt};
use tuwunel_core::{
	Result, implement,
	utils::stream::{ReadyExt, TryIgnore},
};

use crate::keyval::Key;

/// Deletes all entries that exist when the clear scan begins.
///
/// The operation scans a consistent iterator view, so later writes can remain.
/// When debug assertions are disabled, scan errors are filtered after any
/// preceding keys have been removed.
///
/// # Panics
///
/// Panics if a scan error occurs with debug assertions enabled, RocksDB rejects
/// a deletion, or an uncorked flush fails.
#[implement(super::Map)]
#[tracing::instrument(level = "trace")]
pub async fn clear(self: &Arc<Self>) {
	self.for_clear()
		.ignore_err()
		.ready_for_each(|_| ())
		.await;
}

/// Deletes each entry visible to a clear scan and yields its key.
///
/// The iterator view is fixed when the stream begins, so later writes can
/// remain. Polling drives deletion and exposes scan errors to the caller. Each
/// yielded key borrows cursor storage and must not be retained across another
/// poll.
///
/// # Panics
///
/// Panics if RocksDB rejects a deletion or an uncorked flush fails.
#[implement(super::Map)]
#[tracing::instrument(level = "trace")]
pub fn for_clear(self: &Arc<Self>) -> impl Stream<Item = Result<Key<'_>>> + Send {
	self.raw_keys().inspect_ok(|key| self.remove(key))
}
