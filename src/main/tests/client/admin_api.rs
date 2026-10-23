//! Synapse admin API requests for the tests that declare this module by path.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it; the harness's other users stay free of its dead code. A
//! binary declaring it also declares the client harness, whose `Client` it
//! extends.

use reqwest::Method;
use serde_json::Value;
use tuwunel_core::{Result, implement};

use super::client::Client;

/// Send a JSON body to one Synapse admin API path as this user and require the
/// `expected` status.
///
/// The path is what follows `/_synapse/admin/`, version included, and the
/// reply's body comes back as text. The status assertion carries that body,
/// so a routing or fixture failure reads differently from a refusal a case
/// expects.
#[implement(Client, params = "<'_>")]
pub(crate) async fn admin_api(
	&self,
	method: Method,
	path: &str,
	body: &Value,
	expected: u16,
) -> Result<String> {
	let url = format!("{}/_synapse/admin/{path}", self.base);
	let response = self.request(method, &url, body).await?;
	let status = response.status().as_u16();
	let response = response.text().await?;

	assert_eq!(status, expected, "{url}: {response}");

	Ok(response)
}
