#![cfg(test)]

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tuwunel_core::{Result, ruma::user_id, utils::math::Expected};
use tuwunel_service::Services;

use self::{
	client::{Client, register},
	fixture::boot,
};

#[expect(
	dead_code,
	reason = "Only registration and requests are used from the client harness."
)]
mod client;
mod fixture;

#[test]
fn ignored_user_list_is_limited_to_one_mib() -> Result {
	boot("ignored-user-list-size", ["logging.enable=false"], exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let token = "ignored-user-list-size-test-access-token";
	let owner = register(services, "owner", token).await?;
	let client = Client { services, base, token };
	let ignored = user_id!("@ignored:example.org");
	let url = client.url(&format!("user/{owner}/account_data/m.ignored_user_list"));
	let mut content = json!({"ignored_users": {ignored.as_str(): {}}, "padding": ""});
	let padding = 1_048_576_usize.expected_sub(serde_json::to_vec(&content)?.len());
	content["padding"] = json!("x".repeat(padding));

	let response = client
		.request(Method::PUT, &url, &content)
		.await?;
	assert_eq!(response.status(), StatusCode::OK);

	// Same-length replacement ID, one more byte of content.
	content["ignored_users"] = json!({"@another:example.org": {}});
	content["padding"] = json!("x".repeat(padding.expected_add(1)));
	let response = client
		.request(Method::PUT, &url, &content)
		.await?;
	assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
	assert_eq!(response.json::<Value>().await?["errcode"], "M_TOO_LARGE");
	assert!(
		services
			.users
			.user_is_ignored(ignored, &owner)
			.await
	);

	let other = client.url(&format!("user/{owner}/account_data/org.example.other"));
	let response = client
		.request(Method::PUT, &other, &content)
		.await?;
	assert_eq!(response.status(), StatusCode::OK);
	Ok(())
}
