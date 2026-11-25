#![cfg(test)]

use reqwest::StatusCode;
use serde_json::Value;
use tuwunel_core::Result;
use tuwunel_service::Services;

#[expect(dead_code)] // Only the fixture's listener readiness probe is needed.
mod client;
mod fixture;

#[test]
fn signed_routes_require_credentials_before_parsing_content() -> Result {
	fixture::boot("federation-inbound-parse", [], exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let response = services
		.client
		.clients
		.default
		.put(format!("{base}/_matrix/federation/v1/send/parse-order"))
		.body("{")
		.send()
		.await?;
	assert_eq!(
		response.status(),
		StatusCode::FORBIDDEN,
		"missing signature precedes the body error"
	);
	let error: Value = response.json().await?;
	assert_eq!(error["errcode"], "M_FORBIDDEN", "{error}");
	Ok(())
}
