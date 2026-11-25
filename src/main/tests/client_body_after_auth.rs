#![cfg(test)]

use reqwest::StatusCode;
use serde_json::Value;
use tuwunel_core::Result;
use tuwunel_service::Services;

use self::client::register;

#[expect(dead_code)] // Only registration and listener readiness are needed.
mod client;
mod fixture;

const TOKEN: &str = "client-body-order-test-access-token-01";

#[test]
fn client_bodies_are_parsed_after_authentication() -> Result {
	fixture::boot("client-body-after-auth", [], exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let user = register(services, "parseorder", TOKEN).await?;
	let url = format!("{base}/_matrix/client/v3/user/{user}/account_data/m.test");
	let request = || {
		services
			.client
			.clients
			.default
			.put(url.as_str())
			.body("{")
	};
	let anonymous = request().send().await?;
	assert_eq!(
		anonymous.status(),
		StatusCode::UNAUTHORIZED,
		"missing token precedes the body error"
	);
	let error: Value = anonymous.json().await?;
	assert_eq!(error["errcode"], "M_MISSING_TOKEN", "{error}");

	let authenticated = request().bearer_auth(TOKEN).send().await?;
	assert_eq!(
		authenticated.status(),
		StatusCode::BAD_REQUEST,
		"an authenticated request still parses"
	);
	let error: Value = authenticated.json().await?;
	assert_eq!(error["errcode"], "M_NOT_JSON", "{error}");
	Ok(())
}
