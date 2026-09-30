#![cfg(test)]

use std::{fmt::Display, net::TcpListener};

use futures::future::join;
use reqwest::{RequestBuilder, StatusCode};
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result, err,
	jwt::{EncodingKey, Header, encode},
	ruma::UserId,
};
use tuwunel_service::Services;

use self::{
	appservice::{Bridge, register_appservice},
	client::wait_until_ready,
};

mod appservice;
#[expect(
	dead_code,
	reason = "Only listener readiness is shared with the client API harness."
)]
mod client;

const JWT_SECRET: &str = "jwt-login-deactivated-test-secret";
const PASSWORD: &str = "test-password";
const WRONG_PASSWORD: &str = "not-the-test-password";
const DEACTIVATED: &str = "M_USER_DEACTIVATED";
const LIMITED: &str = "M_LIMIT_EXCEEDED";
const LOGIN_TOKEN: &str = "jwt-login-deactivated-login-token";
const BRIDGE_TOKEN: &str = "jwt-login-deactivated-bridge-token";

const BRIDGE: Bridge<'static> = Bridge {
	id: "jwt-login-deactivated",
	token: BRIDGE_TOKEN,
	sender_localpart: "bridge_bot",
	users: "^@bridged_.*$",
	aliases: None,
};

/// Login refuses deactivated accounts and limits wrong passwords per account.
///
/// A deactivated account keeps its record with an empty password hash, so
/// password login refuses it with `M_USER_DEACTIVATED` before comparing
/// anything, and repeating that refusal never spends the account's
/// failed-attempt limit. JWT and login-token logins, which check no password,
/// must refuse it the same way, as must the JWT stage of UIAA, through which
/// another session could otherwise give it a new password. Appservice login is
/// exempt, so an appservice still signs in its passwordless bot. Wrong passwords past that limit's burst are refused with
/// `M_LIMIT_EXCEEDED`, the correct password included, while a correct
/// password inside the burst still signs in.
#[test]
fn login_refuses_deactivated_accounts_and_limits_wrong_passwords() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={port}"))
		.with_option("listening=true")
		.with_option("log_global_default=false")
		.with_option("jwt.enable=true")
		.with_option(format!("jwt.key=\"{JWT_SECRET}\""))
		.with_option("jwt.format=\"HMAC\"")
		.with_option("jwt.algorithm=\"HS256\"")
		.with_option("jwt.register_user=false")
		.with_option("argon2_m_cost=64")
		.with_option("argon2_t_cost=1");

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;

	runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let exercise = async {
			let outcome = exercise(&services, &base).await;
			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (run, outcome) = join(async_run(&server), exercise).await;

		drop(services);
		async_stop(&server).await?;
		run?;

		outcome
	})
}

async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;

	let server_name = services.globals.server_name();
	let active = UserId::parse_with_server_name("active", server_name)?;
	let deactivated = UserId::parse_with_server_name("deactivated", server_name)?;
	let guessed = UserId::parse_with_server_name("guessed", server_name)?;
	let owner = UserId::parse_with_server_name("owner", server_name)?;

	for user_id in [&active, &deactivated, &guessed, &owner] {
		services
			.users
			.create(user_id, Some(PASSWORD), None)
			.await?;
	}

	services
		.users
		.deactivate_account(&deactivated)
		.await?;

	let (status, session) = jwt_login(services, base, &active).await?;

	expect_signed_in("active account", status, &session)?;

	let (status, body) = jwt_login(services, base, &deactivated).await?;

	expect_error("deactivated account", status, &body, StatusCode::FORBIDDEN, DEACTIVATED)?;

	token_login_refuses_deactivated(services, base, &deactivated).await?;
	appservice_login_admits_its_passwordless_bot(services, base).await?;
	jwt_stage_admits_only_active_subjects(services, base, &session, &active, &deactivated)
		.await?;

	let limit = &services.config.rate_limiting.login.failed;
	let burst = limit.burst_count;

	for attempt in 0..=burst {
		let (status, body) = password_login(services, base, &deactivated, PASSWORD).await?;
		let context = format_args!("password attempt {attempt}");

		expect_error(context, status, &body, StatusCode::FORBIDDEN, DEACTIVATED)?;
	}

	wrong_passwords_are_limited(services, base, &guessed, burst).await?;
	correct_password_inside_the_burst(services, base, &owner, burst).await
}

async fn token_login_refuses_deactivated(
	services: &Services,
	base: &str,
	deactivated: &UserId,
) -> Result {
	_ = services
		.users
		.create_login_token(deactivated, LOGIN_TOKEN);

	let token_login = json!({"type": "m.login.token", "token": LOGIN_TOKEN});
	let (status, body) = login(services, base, &token_login).await?;

	expect_error("login token", status, &body, StatusCode::FORBIDDEN, DEACTIVATED)
}

async fn appservice_login_admits_its_passwordless_bot(services: &Services, base: &str) -> Result {
	register_appservice(services, &BRIDGE).await?;

	let appservice_login = json!({
		"type": "m.login.application_service",
		"identifier": {"type": "m.id.user", "user": BRIDGE.sender_localpart},
	});

	let request = post(services, base, "login", &appservice_login).bearer_auth(BRIDGE_TOKEN);
	let (status, body) = send(request).await?;

	expect_signed_in("appservice bot login", status, &body)
}

async fn jwt_stage_admits_only_active_subjects(
	services: &Services,
	base: &str,
	session: &Value,
	active: &UserId,
	deactivated: &UserId,
) -> Result {
	let access_token = session["access_token"]
		.as_str()
		.ok_or_else(|| err!("the active session has no access token: {session}"))?;

	let (status, body) =
		change_password_by_jwt(services, base, access_token, deactivated).await?;

	expect_error("JWT stage of UIAA", status, &body, StatusCode::FORBIDDEN, DEACTIVATED)?;

	if !services.users.is_deactivated(deactivated).await? {
		return Err!("the JWT stage of UIAA gave {deactivated} a password again");
	}

	let (status, body) = change_password_by_jwt(services, base, access_token, active).await?;

	if status != StatusCode::OK {
		return Err!("the JWT stage of UIAA refused the active {active}: {status} {body}");
	}

	Ok(())
}

async fn change_password_by_jwt(
	services: &Services,
	base: &str,
	access_token: &str,
	subject: &UserId,
) -> Result<(StatusCode, Value)> {
	let auth = jwt_auth(subject)?;
	let password_change = json!({"new_password": "changed-by-jwt", "auth": auth});
	let request =
		post(services, base, "account/password", &password_change).bearer_auth(access_token);

	send(request).await
}

async fn wrong_passwords_are_limited(
	services: &Services,
	base: &str,
	user_id: &UserId,
	burst: u32,
) -> Result {
	for attempt in 0..burst {
		let (status, body) = password_login(services, base, user_id, WRONG_PASSWORD).await?;
		let context = format_args!("wrong password {attempt}");

		expect_error(context, status, &body, StatusCode::FORBIDDEN, "M_FORBIDDEN")?;
	}

	let (status, body) = password_login(services, base, user_id, WRONG_PASSWORD).await?;
	let limited = StatusCode::TOO_MANY_REQUESTS;

	expect_error("wrong password past the burst", status, &body, limited, LIMITED)?;

	if body["retry_after_ms"].as_u64().is_none() {
		return Err!("the limited refusal stated no retry_after_ms: {body}");
	}

	let (status, body) = password_login(services, base, user_id, PASSWORD).await?;

	expect_error("correct password past the burst", status, &body, limited, LIMITED)
}

async fn correct_password_inside_the_burst(
	services: &Services,
	base: &str,
	user_id: &UserId,
	burst: u32,
) -> Result {
	for attempt in 1..burst {
		let (status, body) = password_login(services, base, user_id, WRONG_PASSWORD).await?;
		let context = format_args!("wrong password {attempt} inside the burst");

		expect_error(context, status, &body, StatusCode::FORBIDDEN, "M_FORBIDDEN")?;
	}

	let (status, body) = password_login(services, base, user_id, PASSWORD).await?;

	expect_signed_in("correct password inside the burst", status, &body)
}

async fn jwt_login(
	services: &Services,
	base: &str,
	user_id: &UserId,
) -> Result<(StatusCode, Value)> {
	login(services, base, &jwt_auth(user_id)?).await
}

fn jwt_auth(user_id: &UserId) -> Result<Value> {
	let claims = json!({"sub": user_id.localpart()});

	encode(&Header::default(), &claims, &EncodingKey::from_secret(JWT_SECRET.as_bytes()))
		.map(|token| json!({"type": "org.matrix.login.jwt", "token": token}))
		.map_err(|error| err!("failed to mint a JWT: {error}"))
}

async fn password_login(
	services: &Services,
	base: &str,
	user_id: &UserId,
	password: &str,
) -> Result<(StatusCode, Value)> {
	let body = json!({
		"type": "m.login.password",
		"identifier": {"type": "m.id.user", "user": user_id.localpart()},
		"password": password,
	});

	login(services, base, &body).await
}

async fn login(services: &Services, base: &str, body: &Value) -> Result<(StatusCode, Value)> {
	send(post(services, base, "login", body)).await
}

fn post(services: &Services, base: &str, path: &str, body: &Value) -> RequestBuilder {
	services
		.client
		.clients
		.default
		.post(format!("{base}/_matrix/client/v3/{path}"))
		.json(body)
}

async fn send(request: RequestBuilder) -> Result<(StatusCode, Value)> {
	let response = request.send().await?;
	let status = response.status();
	let body: Value = response.json().await?;

	Ok((status, body))
}

fn expect_signed_in<Context>(context: Context, status: StatusCode, body: &Value) -> Result
where
	Context: Display,
{
	if status != StatusCode::OK || body["access_token"].as_str().is_none() {
		return Err!("{context}: expected 200 with an access token, got {status}: {body}");
	}

	Ok(())
}

fn expect_error<Context>(
	context: Context,
	status: StatusCode,
	body: &Value,
	expected: StatusCode,
	errcode: &str,
) -> Result
where
	Context: Display,
{
	if status != expected || body["errcode"] != errcode {
		return Err!("{context}: expected {expected} {errcode}, got {status}: {body}");
	}

	Ok(())
}
