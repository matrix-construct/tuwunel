#![cfg(test)]

use std::net::TcpListener;

use futures::future::join;
use reqwest::StatusCode;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result, err,
	jwt::{EncodingKey, Header, encode},
	ruma::UserId,
};
use tuwunel_service::Services;

use self::client::wait_until_ready;

#[expect(
	dead_code,
	reason = "Only listener readiness is shared with the client API harness."
)]
mod client;

const JWT_SECRET: &str = "jwt-login-deactivated-test-secret";

/// A deactivated account keeps its record with an empty password hash. Password
/// login refuses it with `M_USER_DEACTIVATED` before comparing anything; JWT
/// login must refuse it the same way instead of issuing a session.
#[test]
fn jwt_login_refuses_a_deactivated_account() -> Result {
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
		.with_option("jwt.register_user=false");

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

	for user_id in [&active, &deactivated] {
		services
			.users
			.create(user_id, Some("test-password"), None)
			.await?;
	}

	services
		.users
		.deactivate_account(&deactivated)
		.await?;

	let (status, body) = jwt_login(services, base, &active).await?;
	if status != StatusCode::OK {
		return Err!("active account: expected 200, got {status}: {body}");
	}

	let (status, body) = jwt_login(services, base, &deactivated).await?;
	if status != StatusCode::FORBIDDEN || body["errcode"] != "M_USER_DEACTIVATED" {
		return Err!(
			"deactivated account: expected 403 M_USER_DEACTIVATED, got {status}: {body}"
		);
	}

	Ok(())
}

async fn jwt_login(
	services: &Services,
	base: &str,
	user_id: &UserId,
) -> Result<(StatusCode, Value)> {
	let claims = json!({"sub": user_id.localpart()});
	let token =
		encode(&Header::default(), &claims, &EncodingKey::from_secret(JWT_SECRET.as_bytes()))
			.map_err(|error| err!("failed to mint a JWT login token: {error}"))?;

	let response = services
		.client
		.clients
		.default
		.post(format!("{base}/_matrix/client/v3/login"))
		.json(&json!({"type": "org.matrix.login.jwt", "token": token}))
		.send()
		.await?;

	let status = response.status();
	let body: Value = response.json().await?;

	Ok((status, body))
}
