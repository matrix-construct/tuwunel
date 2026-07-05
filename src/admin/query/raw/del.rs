use tokio::time::Instant;
use tuwunel_core::Result;

use super::decode;
use crate::admin_command;

#[admin_command]
pub(super) async fn raw_del(&self, map: String, key: String) -> Result {
	let map = self.services.db.get(&map)?;
	let timer = Instant::now();

	let key = decode(&key);
	map.remove(&key);

	let query_time = timer.elapsed();
	write!(self, "Operation completed in {query_time:?}").await
}
