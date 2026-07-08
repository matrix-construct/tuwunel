use tokio::time::Instant;
use tuwunel_core::Result;

use super::decode;
use crate::admin_command;

#[admin_command]
pub(super) async fn raw_put(&self, map: String, key: String, value: String) -> Result {
	let map = self.services.db.get(&map)?;
	let timer = Instant::now();

	let key = decode(&key);
	let value = decode(&value);
	map.insert(&key, &value);

	let query_time = timer.elapsed();
	write!(self, "Operation completed in {query_time:?}").await
}
