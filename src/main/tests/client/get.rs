//! GET requests for the tests that declare this module by path.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it; the harness's other users stay free of its dead code. A
//! binary declaring it also declares the client harness, whose `Client` it
//! extends.

use reqwest::{Method, Response};
use serde_json::{Value, json};
use tuwunel_core::{Result, implement};

use super::client::{Client, accepted};

/// Get one endpoint path as this user and parse the reply.
///
/// The GET twin of `post`; a non-success status is the error.
#[implement(Client, params = "<'_>")]
pub(crate) async fn get(&self, path: &str) -> Result<Value> {
	let response = self.get_url(&self.url(path)).await?;

	accepted(response).await
}

/// Send a GET request to an absolute URL as this user and keep the raw reply.
///
/// The GET twin of `post_url`; the status stays the caller's to judge, so a
/// refusal is as easy to assert as a success.
#[implement(Client, params = "<'_>")]
pub(crate) async fn get_url(&self, url: &str) -> Result<Response> {
	self.request(Method::GET, url, &json!({})).await
}
