use base64::prelude::*;
use tokio::time::Instant;
use tuwunel_core::Result;

use super::{decode, encode};
use crate::admin_command;

#[admin_command]
pub(super) async fn raw_get(&self, map: String, key: String, base64: bool) -> Result {
	let map = self.services.db.get(&map)?;
	let timer = Instant::now();

	let key = decode(&key);
	let handle = map.get(&key).await?;

	let query_time = timer.elapsed();

	let result = if base64 {
		BASE64_STANDARD.encode(&handle)
	} else {
		encode(&handle)
	};

	write!(self, "Query completed in {query_time:?}:\n\n```rs\n{result:?}\n```").await
}
