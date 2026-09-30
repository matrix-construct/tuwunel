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

/// A base64-encoded HMAC key. Whole four-character groups need no padding.
const JWT_KEY_BASE64: &str = "dHV3dW5lbC1qd3Qta2V5LWZvcm1hdC10ZXN0";

/// `B64HMAC` is the key format the configuration reference and
/// `docs/authentication/jwt.md` name for a base64 key. Both it and the
/// `HMACB64` spelling accepted before must verify a token signed with the
/// decoded key.
#[test]
fn jwt_base64_key_formats_are_accepted() -> Result {
	for format in ["B64HMAC", "HMACB64"] {
		exercise_server(format)?;
	}

	Ok(())
}

fn exercise_server(format: &str) -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={port}"))
		.with_option("listening=true")
		.with_option("log_global_default=false")
		.with_option("jwt.enable=true")
		.with_option(format!("jwt.key=\"{JWT_KEY_BASE64}\""))
		.with_option(format!("jwt.format=\"{format}\""))
		.with_option("jwt.algorithm=\"HS256\"")
		.with_option("jwt.register_user=false");

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;

	runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let exercise = async {
			let outcome = exercise(&services, &base, format).await;
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

async fn exercise(services: &Services, base: &str, format: &str) -> Result {
	wait_until_ready(services, base).await?;

	let user_id = UserId::parse_with_server_name("keyformat", services.globals.server_name())?;
	services
		.users
		.create(&user_id, Some("test-password"), None)
		.await?;

	let key = EncodingKey::from_base64_secret(JWT_KEY_BASE64)
		.map_err(|error| err!("the test key is not valid base64: {error}"))?;
	let token = encode(&Header::default(), &json!({"sub": user_id.localpart()}), &key)
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

	if status != StatusCode::OK {
		return Err!("jwt.format = {format}: expected 200, got {status}: {body}");
	}

	Ok(())
}
