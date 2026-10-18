//! Raw GET requests for the tests that declare this module by path.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it; the harness's other users stay free of its dead code. A
//! binary declaring it also declares the client harness, whose `Client` it
//! extends.

use reqwest::{Method, Response};
use serde_json::json;
use tuwunel_core::{Result, implement};

use super::client::Client;

/// Send a GET request to an absolute URL as this user and keep the raw reply.
///
/// The GET twin of `post_url`; the status stays the caller's to judge, so a
/// refusal is as easy to assert as a success.
#[implement(Client, params = "<'_>")]
pub(crate) async fn get_url(&self, url: &str) -> Result<Response> {
	self.request(Method::GET, url, &json!({})).await
}
