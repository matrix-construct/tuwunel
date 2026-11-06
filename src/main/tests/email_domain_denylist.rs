#![cfg(test)]

use std::time::Duration;

use reqwest::{Client, Response};
use serde_json::{Value, json};
use tuwunel_core::{
	Result,
	ruma::{UserId, thirdparty::Medium},
};
use tuwunel_service::Services;

#[expect(dead_code)]
mod client;
mod fixture;

use self::{client::register, fixture::boot};

const ACCESS_TOKEN: &str = "email-domain-owner-access-token-at-least-32";
const SECRET: &str = "email-domain-test-secret";

#[test]
fn email_domain_policy_applies_before_sending_and_binding() -> Result {
	let config = [
		"smtp.connection_uri='smtp://127.0.0.1:1'",
		"smtp.sender='test@example.test'",
		"well_known.client='https://example.test'",
		"smtp.require_email_for_registration=true",
		"smtp.forbidden_email_domains=['(^|[.])blocked[.]test$', '^strasse[.]test$']",
		"registration.enable=true",
	];

	boot("email-domain-denylist", config, exercise)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, base: &str) -> Result {
	let http = Client::new();
	let owner = register(services, "email_owner", ACCESS_TOKEN).await?;

	for path in ["register/email/requestToken", "account/3pid/email/requestToken"] {
		for (email, expected_status, expected_code) in [
			("a+tag@BLOCKED.TEST", 403, "M_THREEPID_DENIED"),
			("b@sub.blocked.test", 403, "M_THREEPID_DENIED"),
			("c@straße.test", 403, "M_THREEPID_DENIED"),
			("a b@blocked.test", 400, "M_INVALID_PARAM"),
		] {
			let body = json!({"email": email, "client_secret": SECRET, "send_attempt": 1});
			let response = http
				.post(format!("{base}/_matrix/client/v3/{path}"))
				.json(&body)
				.send()
				.await?;

			let (status, response) = decode(response).await?;

			assert_eq!(status, expected_status, "{response}");
			assert_eq!(response["errcode"], expected_code);
		}
	}

	let sid = seed(services, "old@blocked.test").await?;
	let body = json!({
		"username": "denied_registration", "password": "registration-test-password",
		"auth": {"type": "m.login.email.identity",
			"threepid_creds": {"sid": sid, "client_secret": SECRET}},
	});

	let response = http
		.post(format!("{base}/_matrix/client/v3/register"))
		.json(&body)
		.send()
		.await?;

	let (status, response) = decode(response).await?;
	let denied =
		UserId::parse_with_server_name("denied_registration", services.globals.server_name())?;

	assert_eq!(status, 403, "{response}");
	assert_eq!(response["errcode"], "M_THREEPID_DENIED");
	assert!(!services.users.exists(&denied).await);

	let sid = seed(services, "add@blocked.test").await?;
	let body = json!({
		"sid": sid, "client_secret": SECRET,
		"auth": {"type": "m.login.password", "password": "tuwunel-test-harness-password",
			"identifier": {"type": "m.id.user", "user": owner}},
	});

	let response = http
		.post(format!("{base}/_matrix/client/v3/account/3pid/add"))
		.bearer_auth(ACCESS_TOKEN)
		.json(&body)
		.send()
		.await?;

	let (status, response) = decode(response).await?;

	assert_eq!(status, 403, "{response}");
	assert_eq!(response["errcode"], "M_THREEPID_DENIED");
	assert!(
		services
			.threepid
			.user_id_for_email("add@blocked.test")
			.await?
			.is_none()
	);

	let sid = seed(services, "allowed@notblocked.test").await?;
	let body = json!({
		"username": "allowed_registration", "password": "registration-test-password",
		"inhibit_login": true,
		"auth": {"type": "m.login.email.identity",
			"threepid_creds": {"sid": sid, "client_secret": SECRET}},
	});

	let response = http
		.post(format!("{base}/_matrix/client/v3/register"))
		.json(&body)
		.send()
		.await?;

	let (status, response) = decode(response).await?;

	assert_eq!(status, 200, "{response}");
	assert!(
		services
			.threepid
			.user_id_for_email("allowed@notblocked.test")
			.await?
			.is_some()
	);

	Ok(())
}

#[tracing::instrument(level = "debug", skip_all)]
async fn decode(response: Response) -> Result<(u16, Value)> {
	let status = response.status().as_u16();
	let body = response.json().await?;

	Ok((status, body))
}

#[tracing::instrument(level = "debug", skip_all)]
async fn seed(services: &Services, email: &str) -> Result<String> {
	let pending = services
		.threepid
		.create_or_reuse_pending(SECRET, Medium::Email, email, 1, Duration::from_mins(5))
		.await?;

	let token = pending
		.freshly_minted_token
		.as_deref()
		.expect("fresh token");

	services
		.threepid
		.validate_pending_token(&pending.sid, SECRET, token)
		.await?;

	Ok(pending.sid)
}
