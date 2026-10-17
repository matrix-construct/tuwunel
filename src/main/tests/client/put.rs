//! Parsed PUT requests for the tests that declare this module by path.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it; the harness's other users stay free of its dead code. A
//! binary declaring it also declares the client harness, whose `Client` it
//! extends.

use reqwest::Method;
use serde_json::Value;
use tuwunel_core::{Result, implement};

use super::client::{Client, accepted};

/// Put a JSON body to one endpoint path as this user and parse the reply.
///
/// The PUT twin of `post`; a non-success status is the error.
#[implement(Client, params = "<'_>")]
pub(crate) async fn put(&self, path: &str, body: &Value) -> Result<Value> {
	let response = self
		.request(Method::PUT, &self.url(path), body)
		.await?;

	accepted(response).await
}
