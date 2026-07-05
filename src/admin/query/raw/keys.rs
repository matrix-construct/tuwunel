use futures::{FutureExt, StreamExt, TryStreamExt};
use tokio::time::Instant;
use tuwunel_core::{Result, utils::TryReadyExt};

use super::{decode, encode};
use crate::admin_command;

#[admin_command]
pub(super) async fn raw_keys(
	&self,
	map: String,
	prefix: Option<String>,
	limit: Option<usize>,
	from: Option<String>,
	backwards: bool,
) -> Result {
	writeln!(self, "```").boxed().await?;

	let map = self.services.db.get(map.as_str())?;
	let timer = Instant::now();

	let prefix = prefix.as_deref().map(decode);
	let from = from.as_deref().map(decode);

	let stream = match from.as_ref().or(prefix.as_ref()) {
		| Some(from) if !backwards => map.raw_keys_from(from).boxed(),
		| Some(from) => map.rev_raw_keys_from(from).boxed(),
		| None if !backwards => map.raw_keys().boxed(),
		| None => map.rev_raw_keys().boxed(),
	};

	stream
		.ready_try_take_while(|k| {
			Ok(prefix
				.as_ref()
				.map(|prefix| k.starts_with(prefix))
				.unwrap_or(true))
		})
		.take(limit.unwrap_or(usize::MAX))
		.map_ok(encode)
		.try_for_each(|str| writeln!(self, "{str}"))
		.boxed()
		.await?;

	let query_time = timer.elapsed();
	write!(self, "\n```\n\nQuery completed in {query_time:?}").await
}
