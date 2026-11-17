#![cfg(test)]

use reqwest::{RequestBuilder, StatusCode};
use serde_json::Value;
use tuwunel_core::{Err, Result};
use tuwunel_service::Services;

use self::{client::register, fixture::boot};

#[expect(
	dead_code,
	reason = "Only registration and listener readiness are shared with the client API harness."
)]
mod client;

mod fixture;

const TOKEN: &str = "query-string-token-test-device-token";

/// An access token in the query string authenticates nothing (MSC4127).
///
/// The same token in the `Authorization` header is the positive control, so
/// the refusal is the transport's and not the token's.
#[test]
fn query_string_token_is_refused() -> Result { boot("query-string-token", [], exercise) }

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, base: &str) -> Result {
	let user_id = register(services, "querytoken", TOKEN).await?;
	let http = &services.client.clients.default;
	let whoami = format!("{base}/_matrix/client/v3/account/whoami");

	let query = http
		.get(&whoami)
		.query(&[("access_token", TOKEN)]);

	let (status, body) = reply(query).await?;

	if status != StatusCode::UNAUTHORIZED || body["errcode"] != "M_MISSING_TOKEN" {
		return Err!("expected 401 M_MISSING_TOKEN for a query token, got {status}: {body}");
	}

	let (status, body) = reply(http.get(&whoami).bearer_auth(TOKEN)).await?;

	if status != StatusCode::OK || body["user_id"] != user_id.as_str() {
		return Err!("expected 200 for the same token as a header, got {status}: {body}");
	}

	Ok(())
}

#[tracing::instrument(level = "debug", skip_all)]
async fn reply(request: RequestBuilder) -> Result<(StatusCode, Value)> {
	let response = request.send().await?;
	let status = response.status();
	let body = response.json().await?;

	Ok((status, body))
}
