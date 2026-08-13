//! Insert a Key+Value into the database.
//!
//! Overloads are provided for the user to choose the most efficient
//! serialization or bypass for pre=serialized (raw) inputs.

use tuwunel_core::implement;

use crate::util::or_else;

/// Stores a raw key and raw value in this map.
///
/// The write uses the map's configured options, flushes immediately when the
/// engine is uncorked, and then notifies matching watchers. When the engine is
/// corked, the surrounding cork controls flushing.
///
/// # Panics
///
/// Panics if RocksDB rejects the write or an uncorked flush fails.
#[implement(super::Map)]
#[tracing::instrument(skip_all, fields(%self), level = "trace")]
pub fn insert<K, V>(&self, key: &K, val: V)
where
	K: AsRef<[u8]> + ?Sized,
	V: AsRef<[u8]>,
{
	let write_options = &self.write_options;
	self.engine
		.db
		.put_cf_opt(&self.cf(), key, val, write_options)
		.or_else(or_else)
		.expect("database insert error");

	if !self.engine.corked() {
		self.engine.flush().expect("database flush error");
	}

	self.notify(key.as_ref());
}
