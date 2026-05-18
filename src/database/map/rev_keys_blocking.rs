use std::sync::Arc;

use tuwunel_core::{Result, implement};

use crate::util::map_err;

/// Synchronously iterate all keys in this column family in reverse order.
/// Used at service startup where async iteration is unavailable. Each key is
/// copied out of the rocksdb iterator buffer so the caller may hold it across
/// iteration steps.
#[implement(super::Map)]
pub fn rev_raw_keys_blocking(
	self: &Arc<Self>,
) -> impl Iterator<Item = Result<Box<[u8]>>> + '_ {
	let opts = super::iter_options_default(&self.engine);
	let mut iter = self
		.engine
		.db
		.raw_iterator_cf_opt(&self.cf(), opts);
	iter.seek_to_last();

	std::iter::from_fn(move || {
		if !iter.valid() {
			return iter.status().err().map(|e| Err(map_err(e)));
		}
		let key = iter.key()?.to_vec().into_boxed_slice();
		iter.prev();
		Some(Ok(key))
	})
}
