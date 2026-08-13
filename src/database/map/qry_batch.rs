use std::{fmt::Debug, sync::Arc};

use futures::{Stream, StreamExt, TryStreamExt};
use serde::Serialize;
use tuwunel_core::{
	Result, implement,
	utils::{
		IterStream,
		stream::{WidebandExt, automatic_amplification, automatic_width},
	},
};

use crate::{Handle, keyval::KeyBuf, ser};

/// Extends a stream of structured keys with serialized batched lookup.
///
/// Input keys are encoded and grouped for the engine's blocking pool. The
/// output stream yields pinned value handles or lookup errors.
pub trait Qry<'a, K, S>
where
	S: Stream<Item = K> + Send + 'a,
	K: Serialize + Debug,
{
	/// Fetches this stream's serialized keys from a map.
	///
	/// The returned stream yields lookup results from automatically sized
	/// batches. Serialization is deferred until the stream is polled.
	///
	/// # Panics
	///
	/// Panics if an input key cannot be serialized.
	fn qry(self, map: &'a Arc<super::Map>) -> impl Stream<Item = Result<Handle<'_>>> + Send + 'a;
}

impl<'a, K, S> Qry<'a, K, S> for S
where
	Self: 'a,
	S: Stream<Item = K> + Send + 'a,
	K: Serialize + Debug + 'a,
{
	#[inline]
	fn qry(self, map: &'a Arc<super::Map>) -> impl Stream<Item = Result<Handle<'_>>> + Send + 'a {
		map.qry_batch(self)
	}
}

/// Fetches a stream of structured keys in serialized asynchronous batches.
///
/// Each batch is encoded, run on the engine's blocking pool, and flattened back
/// into individual lookup results.
///
/// # Panics
///
/// Panics if an input key cannot be serialized.
#[implement(super::Map)]
#[tracing::instrument(skip(self, keys), level = "trace")]
pub(crate) fn qry_batch<'a, S, K>(
	self: &'a Arc<Self>,
	keys: S,
) -> impl Stream<Item = Result<Handle<'_>>> + Send + 'a
where
	S: Stream<Item = K> + Send + 'a,
	K: Serialize + Debug + 'a,
{
	use crate::pool::Get;

	keys.ready_chunks(automatic_amplification())
		.widen_then(automatic_width(), |chunk| {
			let keys = chunk
				.iter()
				.map(ser::serialize_to::<KeyBuf, _>)
				.map(|result| result.expect("failed to serialize query key"))
				.collect();

			self.engine
				.pool
				.execute_get(Get { map: self.clone(), key: keys, res: None })
		})
		.map_ok(|results| results.into_iter().stream())
		.try_flatten()
}
