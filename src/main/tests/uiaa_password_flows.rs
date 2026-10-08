#![cfg(test)]

use std::net::TcpListener;

use futures::future::join;
use reqwest::StatusCode;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{Err, Result, ruma::UserId, utils::BoolExt};
use tuwunel_service::{Services, users::PASSWORD_SENTINEL};

use self::client::wait_until_ready;

#[expect(
	dead_code,
	reason = "Only listener readiness is shared with the client API harness."
)]
mod client;

/// LDAP may supply password UIAA only for LDAP-origin accounts. A real local
/// password remains usable regardless of origin or whether LDAP is enabled.
#[test]
fn uiaa_password_flows_match_account_credentials() -> Result {
	for ldap_enabled in [false, true] {
		if ldap_enabled && !cfg!(feature = "ldap") {
			continue;
		}

		exercise_server(ldap_enabled)?;
	}

	Ok(())
}

fn exercise_server(ldap_enabled: bool) -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={port}"))
		.with_option("listening=true")
		.with_option("log_global_default=false")
		.with_option(format!("ldap.enable={ldap_enabled}"))
		.with_option("jwt.enable=false")
		.with_option("identity_provider.test.client_id=\"uiaa-test-idp\"")
		.with_option("identity_provider.test.client_secret=\"test-secret\"")
		.with_option("identity_provider.test.brand=\"test\"")
		.with_option("identity_provider.test.issuer_url=\"https://idp.invalid\"");

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;

	runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let exercise = async {
			let outcome = exercise(&services, &base, ldap_enabled).await;
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

async fn exercise(services: &Services, base: &str, ldap_enabled: bool) -> Result {
	wait_until_ready(services, base).await?;

	let password = json!([{"stages": ["m.login.password"]}]);
	let sso = json!([{"stages": ["m.login.sso"]}]);
	let password_and_sso = json!([
		{"stages": ["m.login.password"]},
		{"stages": ["m.login.sso"]},
	]);
	let no_flows = json!([]);
	let ldap = if ldap_enabled { &password } else { &no_flows };

	for (localpart, credential, origin, expected_flows) in [
		("local", "test-password", "password", &password),
		("ldap", PASSWORD_SENTINEL, "ldap", ldap),
		("sso", PASSWORD_SENTINEL, "sso", &sso),
		("sso-local", "test-password", "sso", &password_and_sso),
		("passwordless", PASSWORD_SENTINEL, "password", &no_flows),
	] {
		let user_id = UserId::parse_with_server_name(localpart, services.globals.server_name())?;
		services
			.users
			.create(&user_id, Some(credential), Some(origin))
			.await?;

		// Setting a real password changes the origin to password. Retain an
		// SSO origin here to cover accounts with both persisted credentials.
		if origin == "sso" && credential != PASSWORD_SENTINEL {
			services.db["userid_origin"].insert(&user_id, origin);
		}

		let token = format!("uiaa-password-flow-test-access-token-{localpart}");
		services
			.users
			.create_device(&user_id, None, (Some(&token), None), None, None, None)
			.await?;

		let response = services
			.client
			.clients
			.default
			.post(format!("{base}/_matrix/client/v3/account/deactivate"))
			.bearer_auth(&token)
			.json(&json!({}))
			.send()
			.await?;

		if response.status() != StatusCode::UNAUTHORIZED {
			return Err!("{localpart}: expected a UIAA challenge, got {}", response.status());
		}

		let challenge: Value = response.json().await?;
		if challenge["flows"] != *expected_flows {
			return Err!(
				"{localpart}, LDAP enabled={ldap_enabled}: expected flows {expected_flows}, got \
				 {}",
				challenge["flows"]
			);
		}

		if challenge["session"]
			.as_str()
			.is_none_or(str::is_empty)
		{
			return Err!("{localpart}: UIAA challenge omitted its session");
		}

		if origin == "sso"
			&& challenge["params"]["m.login.sso"]["identity_providers"]
				!= json!([{"id": "uiaa-test-idp"}])
		{
			return Err!("{localpart}: SSO challenge lost the configured provider binding");
		}
	}

	exercise_retention(services, base).await
}

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise_retention(services: &Services, base: &str) -> Result {
	let user_id = UserId::parse_with_server_name("retention", services.globals.server_name())?;
	let password = "retention-password";
	let token = "uiaa-retention-test-access-token-valid";

	services
		.users
		.create(&user_id, Some(password), Some("password"))
		.await?;

	let sender = services
		.users
		.create_device(&user_id, None, (Some(token), None), None, None, None)
		.await?;

	let client = &services.client.clients.default;
	let url = format!("{base}/_matrix/client/v3/delete_devices");

	for padding in [0, 8192] {
		let target = services
			.users
			.create_device(&user_id, None, (None, None), None, None, None)
			.await?;

		let initial = json!({"devices": [target], "padding": "x".repeat(padding)});
		let response = client
			.post(&url)
			.bearer_auth(token)
			.json(&initial)
			.send()
			.await?;

		assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

		let challenge: Value = response.json().await?;
		let session = challenge["session"]
			.as_str()
			.expect("UIAA session");

		let retained = services
			.uiaa
			.get_uiaa_request(&user_id, Some(&sender), session);

		assert_eq!(retained.is_some(), padding == 0);
		assert!(
			services
				.users
				.device_exists(&user_id, &target)
				.await
		);

		let auth = json!({
			"type": "m.login.password",
			"identifier": {"type": "m.id.user", "user": user_id},
			"password": password,
			"session": session
		});

		let retry = match padding {
			| 0 => json!({"auth": auth}),
			| _ => json!({"devices": [target], "auth": auth}),
		};

		let response = client
			.post(&url)
			.bearer_auth(token)
			.json(&retry)
			.send()
			.await?;

		assert_eq!(response.status(), StatusCode::OK);
		assert!(
			services
				.users
				.device_exists(&user_id, &target)
				.await
				.is_false()
		);

		assert!(
			services
				.uiaa
				.get_uiaa_request(&user_id, Some(&sender), session)
				.is_none()
		);
	}

	Ok(())
}
