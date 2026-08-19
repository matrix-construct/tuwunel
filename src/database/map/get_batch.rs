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

/// Result container for recursive multi-get DAG traversals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecursiveGetOutput<V, K> {
	/// Values successfully fetched and parsed during traversal.
	pub values: Vec<V>,

	/// Keys requested during traversal that were missing from the database.
	pub missing: Vec<K>,

	/// Indicates whether the traversal stopped early due to node or depth caps.
	pub truncated: bool,
}

/// Performs a recursive breadth-first traversal over database keys.
///
/// Starting from `roots`, each batch of keys is fetched in RocksDB using
/// `batched_multi_get_cf_opt` against a point-in-time snapshot. Returned values
/// are parsed by `parse_value`, and any child keys appended to the sink buffer
/// by `extract_children` are queued for the next level of traversal.
///
/// # Traversal Ordering
/// Results are ordered level-by-level (BFS order). Within a single level,
/// results reflect key sorting order. Note that this is **not** a topological
/// sort.
///
/// # Bounds & Limits
/// Traversal halts early if `max_nodes` (total parsed values) or `max_depth`
/// (BFS depth iterations) is reached, marking `truncated = true` on the
/// returned output.
///
/// # Errors
/// Fails fast on server shutdown, key parsing failure, RocksDB I/O errors, or
/// block corruption.
#[implement(super::Map)]
#[tracing::instrument(skip_all, level = "trace")]
pub async fn recursive_multi_get<K, V, P, F, I>(
	self: &Arc<Self>,
	roots: I,
	max_nodes: Option<usize>,
	max_depth: Option<usize>,
	parse_value: P,
	extract_children: F,
) -> Result<RecursiveGetOutput<V, K>>
where
	K: AsRef<[u8]> + Ord + Hash + Clone + Send + Sync + 'static,
	V: Send + 'static,
	P: Fn(&[u8]) -> Result<V> + Send + Sync + 'static,
	F: Fn(&V, &mut Vec<K>) + Send + Sync + 'static,
	I: IntoIterator<Item = K> + Send + 'static,
{
	let map = self.clone();

	tokio::task::spawn_blocking(move || {
		const SORTED: bool = true;

		map.engine.ctx.server.check_running()?;

		let snapshot = map.engine.db.snapshot();
		let mut read_options = super::options::read_options_default(&map.engine);
		read_options.set_snapshot(&snapshot);

		let mut visited = HashSet::new();
		let mut current_batch = Vec::new();
		for root in roots {
			if visited.insert(root.clone()) {
				current_batch.push(root);
			}
		}

		let mut values = Vec::new();
		let mut missing = Vec::new();
		let mut depth: usize = 0;
		let mut truncated = false;

		while !current_batch.is_empty() {
			if let Some(max_d) = max_depth
				&& depth >= max_d
			{
				truncated = true;
				break;
			}

			// Sort keys for optimal sequential RocksDB multi-get access
			current_batch.sort_unstable_by(|a, b| a.as_ref().cmp(b.as_ref()));

			if max_nodes.is_some_and(|max_n| values.len() >= max_n) {
				truncated = true;
				break;
			}

			let db_results = map.engine.db.batched_multi_get_cf_opt(
				&map.cf(),
				current_batch.iter(),
				SORTED,
				&read_options,
			);

			let mut next_batch = Vec::with_capacity(current_batch.len().saturating_mul(2));

			for (key, result) in current_batch.into_iter().zip(db_results) {
				match result {
					| Ok(Some(slice)) =>
						if max_nodes.is_none_or(|max_n| values.len() < max_n) {
							let parsed_value = parse_value(slice.as_ref())?;
							extract_children(&parsed_value, &mut next_batch);
							values.push(parsed_value);

							if max_nodes.is_some_and(|max_n| values.len() >= max_n) {
								truncated = true;
							}
						} else {
							truncated = true;
						},
					| Ok(None) => {
						missing.push(key);
					},
					| Err(e) => {
						tracing::error!(
							key = ?key.as_ref(),
							%e,
							"RocksDB multi-get failure during recursive DAG traversal"
						);
						return Err(map_err(e));
					},
				}
			}

			// Filter out already visited keys from next_batch while preserving order
			next_batch.retain(|child| visited.insert(child.clone()));

			depth = depth.saturating_add(1);

			if truncated {
				break;
			}

			current_batch = next_batch;

			map.engine.ctx.server.check_running()?;
		}

		Ok(RecursiveGetOutput { values, missing, truncated })
	})
	.await
	.map_err(|e| {
		if e.is_panic() {
			tracing::error!("blocking task panicked during recursive_multi_get");
			std::io::Error::other("recursive_multi_get task panicked")
		} else {
			std::io::Error::other("recursive_multi_get task cancelled")
		}
	})?
}
