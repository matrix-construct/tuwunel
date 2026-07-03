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

fn init<'a>(state: stream::State<'a>, dir: Direction, from: Option<&[u8]>) -> stream::State<'a> {
	match dir {
		| Direction::Forward => state.init_fwd(from),
		| Direction::Reverse => state.init_rev(from),
	}
}
