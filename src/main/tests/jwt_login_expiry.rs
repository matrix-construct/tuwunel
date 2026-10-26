#![cfg(test)]

use std::time::Duration;

use futures::StreamExt;
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::time::{Instant, sleep};
use tuwunel_core::{
	Result, err,
	jwt::{EncodingKey, Header, encode},
	ruma::UserId,
};
use tuwunel_service::Services;

use self::fixture::boot;

#[expect(
	dead_code,
	reason = "Only listener readiness is shared with the client harness."
)]
mod client;
mod fixture;

const SECRET: &str = "jwt-session-expiry-test-secret";
const PASSWORD: &str = "jwt-session-expiry-password";
const TTL: u64 = 2;

fn options() -> Vec<String> {
	vec![
		"logging.global_default=false".into(),
		"jwt.enable=true".into(),
		format!("jwt.key=\"{SECRET}\""),
		"jwt.register_user=false".into(),
		format!("authentication.access_token_ttl={TTL}"),
		"authentication.argon2.m_cost=64".into(),
		"authentication.argon2.t_cost=1".into(),
	]
}

// A JWT session expires without refresh negotiation; expired claims fail and password behavior is preserved.
#[test]
fn jwt_sessions_expire_without_refresh_negotiation() -> Result {
	forced_sessions("jwt-session-expiry-no-refresh", false)
}

// Requesting refresh cannot bypass forced JWT expiry or obtain a refresh token.
#[test]
fn jwt_sessions_cannot_request_refresh_tokens() -> Result {
	forced_sessions("jwt-session-expiry-refresh", true)
}

fn forced_sessions(name: &str, refresh: bool) -> Result {
	let mut options = options();
	options.push("jwt.expire_access_token=true".into());
	boot(
		name,
		options.iter().map(String::as_str),
		async move |services: &Services, base: &str| {
			let user = UserId::parse_with_server_name("expiry", services.globals.server_name())?;
			services
				.users
				.create(&user, Some(PASSWORD), None)
				.await?;

			let mut expired = jwt_login(user.localpart(), Some(1))?;
			expired["refresh_token"] = json!(true);
			expired["device_id"] = json!("EXPIRED_CLAIM");
			let (status, response) = login(services, base, &expired).await?;
			assert_eq!(status, StatusCode::FORBIDDEN);
			assert_eq!(response["errcode"], "M_FORBIDDEN");
			assert!(
				!services
					.users
					.device_exists(&user, "EXPIRED_CLAIM".into())
					.await
			);
			assert!(response.get("access_token").is_none());
			assert!(response.get("refresh_token").is_none());

			for _ in 0..2 {
				let mut request = jwt_login(user.localpart(), None)?;
				request["device_id"] = json!("JWT_DEVICE");
				if refresh {
					request["refresh_token"] = json!(true);
				}
				let (status, response) = login(services, base, &request).await?;
				assert_eq!(status, StatusCode::OK);
				assert_eq!(response["expires_in_ms"], TTL * 1000);
				assert!(response.get("refresh_token").is_none(), "{response}");
				let token = response["access_token"]
					.as_str()
					.expect("JWT access token");
				let (_, _, expires) = services.users.find_from_token(token).await?;
				assert!(expires.is_some(), "expiry must be persisted");
				assert_eq!(whoami(services, base, token).await?.0, StatusCode::OK);

				let deadline = Instant::now()
					.checked_add(Duration::from_secs(TTL + 1))
					.expect("short expiry deadline fits");
				loop {
					let (status, response) = whoami(services, base, token).await?;
					if status != StatusCode::OK {
						assert_eq!(status, StatusCode::UNAUTHORIZED);
						assert_eq!(response["errcode"], "M_UNKNOWN_TOKEN");
						assert_eq!(response["soft_logout"], true);
						break;
					}
					assert!(Instant::now() < deadline, "JWT session did not expire");
					sleep(Duration::from_millis(25)).await;
				}
			}

			for refresh in [false, true] {
				let request = json!({
					"type": "m.login.password",
					"identifier": {"type": "m.id.user", "user": user},
					"password": PASSWORD,
					"refresh_token": refresh,
				});
				let (status, response) = login(services, base, &request).await?;
				assert_eq!(status, StatusCode::OK);
				expect_legacy(&response, refresh);
			}
			Ok(())
		},
	)
}

// Omitting the option preserves both permanent JWT sessions and requested refresh sessions.
#[test]
fn default_jwt_sessions_preserve_refresh_negotiation() -> Result {
	boot("jwt-session-default", options().iter().map(String::as_str), legacy_sessions)
}

// Explicitly disabling the option preserves the same legacy behavior as its default.
#[test]
fn disabled_jwt_expiry_preserves_refresh_negotiation() -> Result {
	let mut options = options();
	options.push("jwt.expire_access_token=false".into());
	boot("jwt-session-disabled", options.iter().map(String::as_str), legacy_sessions)
}

async fn legacy_sessions(services: &Services, base: &str) -> Result {
	let user = UserId::parse_with_server_name("legacy", services.globals.server_name())?;
	services
		.users
		.create(&user, Some(PASSWORD), None)
		.await?;
	for refresh in [false, true] {
		let mut request = jwt_login(user.localpart(), None)?;
		if refresh {
			request["refresh_token"] = json!(true);
		}
		let (status, response) = login(services, base, &request).await?;
		assert_eq!(status, StatusCode::OK);
		expect_legacy(&response, refresh);
		let token = response["access_token"]
			.as_str()
			.expect("JWT access token");
		let (_, _, expires) = services.users.find_from_token(token).await?;
		assert_eq!(expires.is_some(), refresh);
		assert_eq!(whoami(services, base, token).await?.0, StatusCode::OK);
	}
	Ok(())
}

fn expect_legacy(response: &Value, refresh: bool) {
	if refresh {
		assert_eq!(response["expires_in_ms"], TTL * 1000);
		assert!(response["refresh_token"].as_str().is_some());
	} else {
		assert!(response.get("expires_in_ms").is_none());
		assert!(response.get("refresh_token").is_none());
	}
}

fn jwt_login(subject: &str, expiry: Option<u64>) -> Result<Value> {
	let mut claims = json!({"sub": subject});
	if let Some(expiry) = expiry {
		claims["exp"] = json!(expiry);
	}
	let token = encode(&Header::default(), &claims, &EncodingKey::from_secret(SECRET.as_bytes()))
		.map_err(|error| err!("JWT encoding failed: {error}"))?;
	Ok(json!({"type": "org.matrix.login.jwt", "token": token}))
}

async fn login(services: &Services, base: &str, body: &Value) -> Result<(StatusCode, Value)> {
	let response = services
		.client
		.clients
		.default
		.post(format!("{base}/_matrix/client/v3/login"))
		.json(body)
		.send()
		.await?;
	Ok((response.status(), response.json().await?))
}

async fn whoami(services: &Services, base: &str, token: &str) -> Result<(StatusCode, Value)> {
	let response = services
		.client
		.clients
		.default
		.get(format!("{base}/_matrix/client/v3/account/whoami"))
		.bearer_auth(token)
		.send()
		.await?;
	Ok((response.status(), response.json().await?))
}

// Enabled JWT login retires a prior current refresh credential from either login flow.
#[test]
fn jwt_retires_current_jwt_refresh() -> Result {
	reused_session("jwt-retire-current-jwt", LoginMethod::Jwt, false, true, LoginMethod::Jwt)
}
#[test]
fn jwt_retires_current_password_refresh() -> Result {
	reused_session(
		"jwt-retire-current-password",
		LoginMethod::Password,
		false,
		true,
		LoginMethod::Jwt,
	)
}

// Retirement also removes the spent credential that otherwise replays during grace.
#[test]
fn jwt_retires_spent_jwt_refresh() -> Result {
	reused_session("jwt-retire-spent-jwt", LoginMethod::Jwt, true, true, LoginMethod::Jwt)
}
#[test]
fn jwt_retires_spent_password_refresh() -> Result {
	reused_session(
		"jwt-retire-spent-password",
		LoginMethod::Password,
		true,
		true,
		LoginMethod::Jwt,
	)
}

// Option-off JWT login preserves old refresh credentials; password refresh login still rotates/replays with the option on.
#[test]
fn other_logins_preserve_refresh_and_grace() -> Result {
	reused_session("jwt-keep-refresh", LoginMethod::Jwt, true, false, LoginMethod::Jwt)?;
	reused_session(
		"password-keep-refresh",
		LoginMethod::Password,
		true,
		true,
		LoginMethod::Password,
	)
}

#[derive(Clone, Copy)]
enum LoginMethod {
	Jwt,
	Password,
}

fn reused_session(
	name: &str,
	prior: LoginMethod,
	spent: bool,
	enabled: bool,
	next: LoginMethod,
) -> Result {
	let options = options();
	boot(
		name,
		options.iter().map(String::as_str),
		async move |services: &Services, base: &str| {
			let user = UserId::parse_with_server_name("reused", services.globals.server_name())?;
			services
				.users
				.create(&user, Some(PASSWORD), None)
				.await?;
			let request = |method| -> Result<Value> {
				let mut body = if matches!(method, LoginMethod::Jwt) {
					jwt_login(user.localpart(), None)?
				} else {
					json!({"type":"m.login.password", "identifier":{"type":"m.id.user", "user":user}, "password":PASSWORD})
				};
				body["device_id"] = json!("REUSED");
				body["refresh_token"] = json!(true);
				Ok(body)
			};
			let (status, prior) = login(services, base, &request(prior)?).await?;
			assert_eq!(status, StatusCode::OK);
			let original = prior["refresh_token"]
				.as_str()
				.expect("prior refresh");
			let current = if spent {
				let (status, rotated) = refresh_session(services, base, original).await?;
				assert_eq!(status, StatusCode::OK);
				let (status, replay) = refresh_session(services, base, original).await?;
				assert_eq!(status, StatusCode::OK, "grace positive control");
				assert_eq!(replay["refresh_token"], rotated["refresh_token"]);
				assert_eq!(
					whoami(
						services,
						base,
						replay["access_token"]
							.as_str()
							.expect("replay access")
					)
					.await?
					.0,
					StatusCode::OK
				);
				rotated["refresh_token"]
					.as_str()
					.expect("successor refresh")
					.to_owned()
			} else {
				original.to_owned()
			};
			let path = services
				.config
				.database_path
				.join("jwt-expiry-reload.toml");
			std::fs::write(&path, format!("[global.jwt]\nexpire_access_token={enabled}\n"))?;
			services
				.config
				.reload(std::iter::once(path.as_path()))?;
			std::fs::remove_file(path)?;
			assert_eq!(services.config.jwt.expire_access_token, enabled);
			let mut next_request = request(next)?;
			if matches!(next, LoginMethod::Jwt) && !enabled {
				next_request["refresh_token"] = json!(false);
			}
			let (status, session) = login(services, base, &next_request).await?;
			assert_eq!(status, StatusCode::OK);
			if enabled && matches!(next, LoginMethod::Jwt) {
				assert_eq!(session["expires_in_ms"], TTL * 1000);
				assert!(session.get("refresh_token").is_none());
				for token in [original, current.as_str()] {
					let (status, refused) = refresh_session(services, base, token).await?;
					assert_eq!(
						status,
						StatusCode::FORBIDDEN,
						"old refresh remained usable: {refused}"
					);
					assert_eq!(refused["errcode"], "M_FORBIDDEN");
					assert!(refused.get("access_token").is_none());
					assert!(refused.get("refresh_token").is_none());
					assert!(
						services
							.users
							.classify_refresh_token(token)
							.await
							.owner()
							.is_none()
					);
				}
				for name in [
					"userdeviceid_refresh",
					"userdeviceid_spentrefresh",
					"spentrefresh_userdeviceid",
				] {
					assert!(
						services.db[name]
							.raw_stream()
							.take(1)
							.count()
							.await
							== 0,
						"{name} retained refresh state"
					);
				}
				let access = session["access_token"]
					.as_str()
					.expect("JWT access");
				assert_eq!(whoami(services, base, access).await?.0, StatusCode::OK);
			} else {
				if matches!(next, LoginMethod::Jwt) && !enabled {
					let (status, replay) = refresh_session(services, base, original).await?;
					assert_eq!(status, StatusCode::OK, "option-off preserves spent refresh");
					assert_eq!(replay["refresh_token"], current);
				}
				let (status, renewed) = refresh_session(services, base, &current).await?;
				assert_eq!(status, StatusCode::OK, "{renewed}");
				let token = renewed["access_token"]
					.as_str()
					.expect("renewed access");
				assert_eq!(whoami(services, base, token).await?.0, StatusCode::OK);
				let (status, replay) = refresh_session(services, base, &current).await?;
				assert_eq!(status, StatusCode::OK);
				assert_eq!(replay["refresh_token"], renewed["refresh_token"]);
			}
			Ok(())
		},
	)
}

async fn refresh_session(
	services: &Services,
	base: &str,
	token: &str,
) -> Result<(StatusCode, Value)> {
	let response = services
		.client
		.clients
		.default
		.post(format!("{base}/_matrix/client/v3/refresh"))
		.json(&json!({"refresh_token":token}))
		.send()
		.await?;
	Ok((response.status(), response.json().await?))
}
