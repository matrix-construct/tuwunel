//! Simplified sliding sync requests shared by the tests that opt into it.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it; the harness's other users stay free of its dead code. A
//! binary declaring it also declares the client harness, whose `Client` it
//! extends.

use serde_json::Value;
use tuwunel_core::{Result, implement};

use super::client::Client;

/// One simplified sliding sync request, its reply parsed.
///
/// Without `pos` it opens the connection; with one it resumes there and
/// long-polls for `timeout` milliseconds. A non-success status is the error.
#[implement(Client, params = "<'_>")]
pub(crate) async fn sliding_sync(
	&self,
	pos: Option<&str>,
	timeout: u64,
	body: &Value,
) -> Result<Value> {
	self.post_json(&self.sliding_sync_url(pos, timeout), body)
		.await
}

/// The simplified sliding sync URL for one request of a connection.
///
/// Without `pos` the URL carries no query; with one it carries `pos` and
/// `timeout`.
#[implement(Client, params = "<'_>")]
pub(crate) fn sliding_sync_url(&self, pos: Option<&str>, timeout: u64) -> String {
	let query = pos
		.map(|pos| format!("?pos={pos}&timeout={timeout}"))
		.unwrap_or_default();

	self.client_url("unstable/org.matrix.simplified_msc3575/sync", &query)
}
