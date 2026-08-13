use std::sync::Arc;

use futures::{FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt, future::Either};
use rocksdb::Direction;
use tokio::task;
use tuwunel_core::Result;

use super::{Map, cache_iter_options_default, iter_options_default};
use crate::{
	pool::{Seek, into_send_seek},
	stream,
};

/// Builds a forward or reverse map stream from an optional raw seek key.
///
/// A block-cache probe selects inline iteration when the initial seek is
/// cached; otherwise the seek runs on the engine's blocking pool. The
/// projection type determines whether each item contains a key alone or a
/// key-value pair.
pub(super) fn seek_stream<'a, C, T>(
	map: &'a Arc<Map>,
	dir: Direction,
	from: Option<&[u8]>,
) -> impl Stream<Item = Result<T>> + Send + use<'a, C, T>
where
	C: From<stream::State<'a>> + Stream<Item = Result<T>> + Send,
{
	let opts = iter_options_default(&map.engine);
	let state = stream::State::new(map, opts);
	if is_cached(map, dir, from) {
		let state = init(state, dir, from);
		return Either::Left(
			task::consume_budget()
				.map(move |()| C::from(state))
				.into_stream()
				.flatten(),
		);
	}

	let seek = Seek {
		map: map.clone(),
		state: into_send_seek(state),
		dir,
		key: from.map(Into::into),
		res: None,
	};

	Either::Right(
		map.engine
			.pool
			.execute_iter(seek)
			.ok_into::<C>()
			.into_stream()
			.try_flatten(),
	)
}

/// Tests whether an initial seek can complete from block cache.
///
/// The probe uses the same direction and starting key as the real iterator
/// without filling cache.
#[tracing::instrument(
    name = "cached",
    level = "trace",
    skip_all,
    fields(%map),
)]
fn is_cached(map: &Arc<Map>, dir: Direction, from: Option<&[u8]>) -> bool {
	let opts = cache_iter_options_default(&map.engine);
	let state = init(stream::State::new(map, opts), dir, from);

	!state.is_incomplete()
}

/// Initializes iterator state for the requested seek direction.
///
/// The optional raw key is interpreted as a lower bound when moving forward and
/// an upper bound when moving backward.
fn init<'a>(state: stream::State<'a>, dir: Direction, from: Option<&[u8]>) -> stream::State<'a> {
	match dir {
		| Direction::Forward => state.init_fwd(from),
		| Direction::Reverse => state.init_rev(from),
	}
}
