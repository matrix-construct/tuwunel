use std::{fmt::Debug, sync::Arc};

use serde::Serialize;
use tuwunel_core::{
	implement,
	utils::stream::{ReadyExt, TryIgnore},
};

/// Deletes every key visible under a serialized prefix.
///
/// The operation scans a consistent iterator view, so later writes can remain.
/// When debug assertions are disabled, scan errors are filtered after any
/// preceding keys have been removed.
///
/// # Panics
///
/// Panics if the prefix cannot be serialized, a scan error occurs with debug
/// assertions enabled, RocksDB rejects a deletion, or an uncorked flush fails.
#[implement(super::Map)]
#[tracing::instrument(level = "trace", skip(self))]
pub async fn del_prefix<P>(self: &Arc<Self>, prefix: &P)
where
	P: Serialize + ?Sized + Debug + Sync,
{
	self.keys_prefix_raw(prefix)
		.ignore_err()
		.ready_for_each(|key| self.remove(&key))
		.await;
}
