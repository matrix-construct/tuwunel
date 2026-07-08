use futures::{FutureExt, StreamExt, TryStreamExt};
use tokio::time::Instant;
use tuwunel_core::{Result, apply, utils::TryReadyExt};
use tuwunel_database::KeyVal;

use super::encode;
use crate::admin_command;

#[admin_command]
pub(super) async fn raw_iter(
	&self,
	map: String,
	prefix: Option<String>,
	limit: Option<usize>,
	from: Option<String>,
	backwards: bool,
) -> Result {
	writeln!(self, "```").await?;

	let map = self.services.db.get(&map)?;
	let timer = Instant::now();
	let stream = match from.as_ref().or(prefix.as_ref()) {
		| Some(from) if !backwards => map.raw_stream_from(from).boxed(),
		| Some(from) => map.rev_raw_stream_from(from).boxed(),
		| None if !backwards => map.raw_stream().boxed(),
		| None => map.rev_raw_stream().boxed(),
	};

	let prefix = prefix.as_ref().map(String::as_bytes);

	stream
		.ready_try_take_while(|(k, _): &KeyVal<'_>| {
			Ok(prefix
				.map(|prefix| k.starts_with(prefix))
				.unwrap_or(true))
		})
		.take(limit.unwrap_or(usize::MAX))
		.map_ok(apply!(2, encode))
		.try_for_each(|(key, val)| writeln!(self, "{key} => {val}"))
		.boxed()
		.await?;

	let query_time = timer.elapsed();
	write!(self, "\n```\n\nQuery completed in {query_time:?}").await
}
