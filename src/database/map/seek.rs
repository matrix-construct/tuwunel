use std::sync::Arc;

use futures::{FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt};
use rocksdb::{Direction, ReadOptions};
use tokio::task::consume_budget;
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
	seek_stream_bounded::<C, T>(map, dir, from, None)
}

/// Builds a map stream with an optional exclusive upper bound.
///
/// Both the cache probe and the real iterator own their bound bytes. The
/// direction and starting position retain the unbounded seek behavior.
pub(super) fn seek_stream_bounded<'a, C, T>(
	map: &'a Arc<Map>,
	dir: Direction,
	from: Option<&[u8]>,
	to: Option<&[u8]>,
) -> impl Stream<Item = Result<T>> + Send + use<'a, C, T>
where
	C: From<stream::State<'a>> + Stream<Item = Result<T>> + Send,
{
	let opts = bounded_options(iter_options_default(&map.engine), to);
	let state = stream::State::new(map, opts);
	if is_cached(map, dir, from, to) {
		let state = init(state, dir, from);
		return consume_budget()
			.map(move |()| C::from(state))
			.into_stream()
			.flatten()
			.left_stream();
	}

	let seek = Seek {
		map: map.clone(),
		state: into_send_seek(state),
		dir,
		key: from.map(Into::into),
		res: None,
	};

	map.engine
		.pool
		.execute_iter(seek)
		.ok_into::<C>()
		.into_stream()
		.try_flatten()
		.right_stream()
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
fn is_cached(map: &Arc<Map>, dir: Direction, from: Option<&[u8]>, to: Option<&[u8]>) -> bool {
	let opts = bounded_options(cache_iter_options_default(&map.engine), to);
	let state = init(stream::State::new(map, opts), dir, from);

	!state.is_incomplete()
}

fn bounded_options(mut opts: ReadOptions, to: Option<&[u8]>) -> ReadOptions {
	if let Some(to) = to {
		opts.set_iterate_upper_bound(to);
	}

	opts
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
