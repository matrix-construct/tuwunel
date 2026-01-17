use futures::StreamExt;
use tuwunel_core::Result;
use tuwunel_macros::implement;

use crate::Context;

/// Uses the iterator in `src/database/key_value/users.rs` to iterator over
/// every user in our database (remote and local). Reports total count, any
/// errors if there were any, etc
#[implement(Context, params = "<'_>")]
pub(super) async fn check_all_users(&self) -> Result {
	let timer = tokio::time::Instant::now();
	let users = self
		.services
		.users
		.stream()
		.map(ToOwned::to_owned)
		.collect::<Vec<_>>()
		.await;
	let query_time = timer.elapsed();

	let total = users.len();
	let err_count = users.iter().filter(|_user| false).count();
	let ok_count = users.iter().filter(|_user| true).count();

	self.write_str(&format!(
		"Database query completed in {query_time:?}:\n\n```\nTotal entries: \
		 {total:?}\nFailure/Invalid user count: {err_count:?}\nSuccess/Valid user count: \
		 {ok_count:?}\n```"
	))
	.await
}
