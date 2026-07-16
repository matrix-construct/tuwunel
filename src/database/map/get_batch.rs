use std::sync::Arc;

use futures::{Stream, StreamExt, TryStreamExt};
use rocksdb::{DBPinnableSlice, ReadOptions};
use tuwunel_core::{
	Result, implement,
	utils::{
		IterStream,
		stream::{WidebandExt, automatic_amplification, automatic_width},
	},
};

use super::get::{cached_handle_from, handle_from};
use crate::Handle;

pub trait Get<'a, K, S>
where
	Self: Sized,
	S: Stream<Item = K> + Send + 'a,
	K: AsRef<[u8]> + Send + Sync + 'a,
{
	fn get(self, map: &'a Arc<super::Map>) -> impl Stream<Item = Result<Handle<'_>>> + Send + 'a;
}

impl<'a, K, S> Get<'a, K, S> for S
where
	Self: Sized,
	S: Stream<Item = K> + Send + 'a,
	K: AsRef<[u8]> + Send + Sync + 'a,
{
	#[inline]
	fn get(self, map: &'a Arc<super::Map>) -> impl Stream<Item = Result<Handle<'_>>> + Send + 'a {
		map.get_batch(self)
	}
}

#[implement(super::Map)]
#[tracing::instrument(skip(self, keys), level = "trace")]
pub(crate) fn get_batch<'a, S, K>(
	self: &'a Arc<Self>,
	keys: S,
) -> impl Stream<Item = Result<Handle<'_>>> + Send + 'a
where
	S: Stream<Item = K> + Send + 'a,
	K: AsRef<[u8]> + Send + Sync + 'a,
{
	use crate::pool::Get;

	keys.ready_chunks(automatic_amplification())
		.widen_then(automatic_width(), |chunk| {
			self.engine.pool.execute_get(Get {
				map: self.clone(),
				res: None,
				key: chunk
					.iter()
					.map(AsRef::as_ref)
					.map(Into::into)
					.collect(),
			})
		})
		.map_ok(|results| results.into_iter().stream())
		.try_flatten()
}

#[implement(super::Map)]
#[tracing::instrument(name = "batch_cached", level = "trace", skip_all)]
pub(crate) fn _get_batch_cached<'a, I, K>(
	&self,
	keys: I,
) -> impl Iterator<Item = Result<Option<Handle<'_>>>> + Send + use<'_, I, K>
where
	I: Iterator<Item = &'a K> + ExactSizeIterator + Send,
	K: AsRef<[u8]> + Send + ?Sized + Sync + 'a,
{
	self.get_batch_blocking_opts(keys, &self.cache_read_options)
		.map(cached_handle_from)
}

#[implement(super::Map)]
#[tracing::instrument(name = "batch_blocking", level = "trace", skip_all)]
pub(crate) fn get_batch_blocking<'a, I, K>(
	&self,
	keys: I,
) -> impl Iterator<Item = Result<Handle<'_>>> + Send + use<'_, I, K>
where
	I: Iterator<Item = &'a K> + ExactSizeIterator + Send,
	K: AsRef<[u8]> + Send + ?Sized + Sync + 'a,
{
	self.get_batch_blocking_opts(keys, &self.read_options)
		.map(handle_from)
}

#[implement(super::Map)]
fn get_batch_blocking_opts<'a, I, K>(
	&self,
	keys: I,
	read_options: &ReadOptions,
) -> impl Iterator<Item = Result<Option<DBPinnableSlice<'_>>, rocksdb::Error>> + Send + use<'_, I, K>
where
	I: Iterator<Item = &'a K> + ExactSizeIterator + Send,
	K: AsRef<[u8]> + Send + ?Sized + Sync + 'a,
{
	// Optimization can be `true` if key vector is pre-sorted **by the column
	// comparator**.
	const SORTED: bool = false;

	self.engine
		.db
		.batched_multi_get_cf_opt(&self.cf(), keys, SORTED, read_options)
		.into_iter()
}
