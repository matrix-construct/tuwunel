//! Resumed sliding sync rounds shared by the tests that poll briefly.
//!
//! A test binary declares this file by path, beside the sliding sync module,
//! only when it resumes syncs, so that module's other users compile none of it.

use serde_json::Value;
use tuwunel_core::{Result, implement};

use super::client::{Client, field};

/// How long a resumed round polls, in milliseconds.
///
/// A round expected to carry nothing waits this long for it before answering
/// empty.
const POLL_TIMEOUT: u64 = 100;

/// One sliding sync resumed from the previous reply.
///
/// The opening request answers after one pass and a resumed one polls for
/// `POLL_TIMEOUT`; `previous` supplies the position through its `pos` field.
#[implement(Client, params = "<'_>")]
pub(crate) async fn sync(&self, previous: Option<&Value>, body: &Value) -> Result<Value> {
	let pos = previous
		.map(|response| field(response, "pos"))
		.transpose()?;

	self.sliding_sync(pos, POLL_TIMEOUT, body).await
}
