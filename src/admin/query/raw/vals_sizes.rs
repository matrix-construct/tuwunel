use std::collections::BTreeMap;

use futures::StreamExt;
use tuwunel_core::{
	Result, at,
	utils::{
		stream::{IterStream, ReadyExt, TryIgnore},
		string::EMPTY,
	},
};

use super::{decode, with_map_or};
use crate::admin_command;

#[admin_command]
pub(super) async fn raw_vals_sizes(&self, map: Option<String>, prefix: Option<String>) -> Result {
	let prefix = decode(prefix.as_deref().unwrap_or(EMPTY));

	let maps = with_map_or(map.as_deref(), self.services)?;

	let query = maps
		.iter()
		.stream()
		.map(|map| map.raw_stream_prefix(&prefix))
		.flatten()
		.ignore_err()
		.map(at!(1))
		.map(<[u8]>::len)
		.ready_fold_default(|mut map: BTreeMap<_, usize>, len| {
			let entry = map.entry(len).or_default();
			*entry = entry.saturating_add(1);
			map
		});

	self.write_timed_query(query).await
}
