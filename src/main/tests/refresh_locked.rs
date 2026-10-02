#![cfg(test)]

use reqwest::StatusCode;
use serde_json::{Value, json};
use tuwunel_core::{Err, Result, err, ruma::UserId};
use tuwunel_service::{Services, users::device::RefreshToken};

use self::fixture::boot;

#[expect(
	dead_code,
	reason = "Only listener readiness is shared with the client API harness."
)]
mod client;

mod fixture;

#[test]
fn refresh_refuses_locked_accounts_without_spending_tokens() -> Result {
	let options = ["refresh_token_reuse_grace=3600", "argon2_m_cost=64", "argon2_t_cost=1"];

	boot("refresh-locked", options, exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let user = UserId::parse_with_server_name("locked", services.globals.server_name())?;

	services
		.users
		.create(&user, Some("password"), None)
		.await?;

	let login = json!({
		"type": "m.login.password",
		"identifier": {"type": "m.id.user", "user": user},
		"password": "password",
		"refresh_token": true,
	});

	let (status, session) = post(services, base, "login", &login).await?;

	let original = expect_session(status, &session)?;

	services.users.set_locked(&user, &user);
	expect_locked(services, base, original).await?;
	expect_current(services, original).await;
	services.users.clear_locked(&user);

	let successor = refresh(services, base, original).await?;
	let current = token(&successor)?;

	if current == original {
		return Err!("unlocked current token did not rotate");
	}

	replay_keeps(services, base, original, current).await?;

	services.users.set_locked(&user, &user);
	expect_locked(services, base, original).await?;
	expect_current(services, current).await;
	expect_locked(services, base, current).await?;
	expect_current(services, current).await;
	services.users.clear_locked(&user);

	replay_keeps(services, base, original, current).await?;

	let next = refresh(services, base, current).await?;

	refresh(services, base, token(&next)?).await?;

	Ok(())
}

async fn expect_locked(services: &Services, base: &str, token: &str) -> Result {
	let (status, body) = post_refresh(services, base, token).await?;

	if status != StatusCode::UNAUTHORIZED
		|| body["errcode"] != "M_USER_LOCKED"
		|| body["soft_logout"] != true
	{
		return Err!("expected 401 M_USER_LOCKED with soft_logout, got {status}: {body}");
	}

	Ok(())
}

async fn expect_current(services: &Services, token: &str) {
	let state = services.users.classify_refresh_token(token).await;

	assert!(matches!(state, RefreshToken::Current { .. }), "refresh token was spent");
}

async fn replay_keeps(services: &Services, base: &str, stale: &str, current: &str) -> Result {
	let replay = refresh(services, base, stale).await?;

	if token(&replay)? != current {
		return Err!("grace replay changed the refresh token");
	}

	Ok(())
}

async fn refresh(services: &Services, base: &str, token: &str) -> Result<Value> {
	let (status, body) = post_refresh(services, base, token).await?;

	expect_session(status, &body)?;

	Ok(body)
}

async fn post_refresh(
	services: &Services,
	base: &str,
	token: &str,
) -> Result<(StatusCode, Value)> {
	let request = json!({"refresh_token": token});

	post(services, base, "refresh", &request).await
}

async fn post(
	services: &Services,
	base: &str,
	path: &str,
	body: &Value,
) -> Result<(StatusCode, Value)> {
	let response = services
		.client
		.clients
		.default
		.post(format!("{base}/_matrix/client/v3/{path}"))
		.json(body)
		.send()
		.await?;

	let status = response.status();
	let body = response.json().await?;

	Ok((status, body))
}

fn expect_session(status: StatusCode, body: &Value) -> Result<&str> {
	if status != StatusCode::OK || body["access_token"].as_str().is_none() {
		return Err!("expected 200 with an access token, got {status}: {body}");
	}

	token(body)
}

fn token(body: &Value) -> Result<&str> {
	body["refresh_token"]
		.as_str()
		.ok_or_else(|| err!("response has no refresh token: {body}"))
}
