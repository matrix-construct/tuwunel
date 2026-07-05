use futures::StreamExt;
use tuwunel_core::{
	Result,
	utils::{
		stream::{IterStream, ReadyExt},
		string::EMPTY,
	},
};

use super::{decode, with_map_or};
use crate::admin_command;

#[admin_command]
pub(super) async fn raw_count(&self, map: Option<String>, prefix: Option<String>) -> Result {
	let prefix = decode(prefix.as_deref().unwrap_or(EMPTY));

	let maps = with_map_or(map.as_deref(), self.services)?;

	let query = maps
		.iter()
		.stream()
		.then(|map| map.raw_count_prefix(&prefix))
		.ready_fold(0_usize, usize::saturating_add);

	self.write_timed_query(query).await
}
