use std::{collections::HashSet, hash::Hash, sync::Arc};

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
use crate::{Handle, util::map_err};

/// Extends a stream of raw keys with batched map lookup.
///
/// Input keys are grouped for the engine's blocking pool. The output stream
/// yields pinned value handles or lookup errors.
pub trait Get<'a, K, S>
where
	Self: Sized,
	S: Stream<Item = K> + Send + 'a,
	K: AsRef<[u8]> + Send + Sync + 'a,
{
	/// Fetches this stream's raw keys from a map.
	///
	/// Successful batches yield one lookup result for each input key. A
	/// batch-level pool or channel failure appears as one stream error for that
	/// batch. Work is split into batches sized from the server's automatic
	/// amplification setting.
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

/// Fetches a stream of raw keys in asynchronous batches.
///
/// Each batch runs on the engine's blocking pool and is flattened back into
/// individual lookup results.
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

/// Fetches an exact-size raw-key iterator from block cache.
///
/// Cache misses remain `Ok(None)`, while cached values and failures retain
/// their normal result forms.
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

/// Fetches an exact-size raw-key iterator synchronously.
///
/// RocksDB performs a batched multi-get and the returned iterator classifies
/// each point-read result.
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

/// Performs a batched multi-get with explicit RocksDB read options.
///
/// Keys are treated as unsorted because callers do not promise
/// column-comparator order.
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

/// Performs a recursive breadth-first traversal over database keys.
///
/// Starting from `roots`, each batch of keys is fetched in RocksDB using
/// `batched_multi_get_cf_opt`. Returned values are parsed by `parse_value`,
/// and any child keys returned by `extract_children` are queued for the next
/// level of traversal.
///
/// # Errors
///
/// Fails fast on any underlying RocksDB I/O or block corruption error.
#[implement(super::Map)]
#[tracing::instrument(skip_all, level = "trace")]
pub async fn recursive_multi_get<K, V, P, F, I>(
	self: &Arc<Self>,
	roots: I,
	parse_value: P,
	extract_children: F,
) -> Result<Vec<V>>
where
	K: AsRef<[u8]> + Eq + Hash + Clone + Send + Sync + 'static,
	V: Send + 'static,
	P: Fn(&[u8]) -> V + Send + Sync + 'static,
	F: Fn(&V) -> Vec<K> + Send + Sync + 'static,
	I: IntoIterator<Item = K>,
{
	let map = self.clone();
	let mut current_batch: Vec<K> = roots.into_iter().collect();

	tokio::task::spawn_blocking(move || {
		let mut results = Vec::new();
		let mut visited = HashSet::new();

		for root in &current_batch {
			visited.insert(root.clone());
		}

		while !current_batch.is_empty() {
			const SORTED: bool = false;
			let db_results = map.engine.db.batched_multi_get_cf_opt(
				&map.cf(),
				current_batch.iter(),
				SORTED,
				&map.read_options,
			);

			let mut next_batch = Vec::with_capacity(current_batch.len() * 2);

			for result in db_results {
				match result {
					| Ok(Some(slice)) => {
						let parsed_value = parse_value(slice.as_ref());
						let children = extract_children(&parsed_value);

						results.push(parsed_value);

						for child in children {
							if visited.insert(child.clone()) {
								next_batch.push(child);
							}
						}
					},
					| Ok(None) => {},
					| Err(e) => {
						tracing::error!(%e, "RocksDB multi-get failure during recursive DAG traversal");
						return Err(map_err(e));
					},
				}
			}

			current_batch = next_batch;
		}

		Ok(results)
	})
	.await
	.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?
}
