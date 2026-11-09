#![cfg(test)]

use std::net::TcpListener;

use futures::future::join;
use reqwest::StatusCode;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{Err, Result, ruma::UserId};
use tuwunel_service::{Services, oauth::Session, users::PASSWORD_SENTINEL};

use self::{client::wait_until_ready, fixture::boot, uiaa_signing::exercise as exercise_signing};

#[expect(
	dead_code,
	reason = "Only listener readiness is shared with the client API harness."
)]
mod client;
mod fixture;
mod uiaa_signing;

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

/// SSO re-authentication binds an untagged device to a provider its account links.
///
/// With several providers configured, the default one wins among those linked,
/// then the first in configured order, and a device's own OIDC tag wins over
/// both. An account linked to none, or only to a provider no longer configured,
/// is offered no SSO flow.
#[test]
fn uiaa_sso_binds_a_linked_provider() -> Result {
	let providers = ["alpha", "beta", "gamma"]
		.into_iter()
		.flat_map(|name| {
			[
				format!("identity_provider.{name}.client_id=\"{name}\""),
				format!("identity_provider.{name}.client_secret=\"test-secret\""),
				format!("identity_provider.{name}.brand=\"{name}-brand\""),
				format!("identity_provider.{name}.issuer_url=\"https://{name}.invalid\""),
				format!("identity_provider.{name}.discovery=false"),
			]
		});

	let options: Vec<_> = [
		"logging.global_default=false".to_owned(),
		"jwt.enable=false".to_owned(),
		"identity_provider.gamma.default=true".to_owned(),
	]
	.into_iter()
	.chain(providers)
	.collect();

	boot("uiaa-linked-provider", options.iter().map(String::as_str), exercise_linked)
}

fn exercise_server(ldap_enabled: bool) -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let args = Args::default_test(&["fresh", "cleanup"])
		.with_loopback_listener(port)
		.with_option("listening=true")
		.with_option("logging.global_default=false")
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

		let challenge = deactivate_challenge(services, base, &user_id, None).await?;

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

	exercise_retention(services, base).await?;

	exercise_signing(services, base).await
}

/// Request account deactivation from a new device of the user, without auth.
///
/// The device carries the OIDC provider tag given, if any. Anything but a 401
/// carrying the UIAA challenge is an error.
#[tracing::instrument(level = "debug", skip_all)]
async fn deactivate_challenge(
	services: &Services,
	base: &str,
	user_id: &UserId,
	tag: Option<&str>,
) -> Result<Value> {
	let localpart = user_id.localpart();
	let token = format!("uiaa-password-flow-test-access-token-{localpart}");
	let device_id = services
		.users
		.create_device(user_id, None, (Some(&token), None), None, None, None)
		.await?;

	if let Some(idp_id) = tag {
		services
			.users
			.mark_oidc_device(user_id, &device_id, idp_id);
	}

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

	Ok(response.json().await?)
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

	// An auth-only retry, a body too large to retain, and an emptied list.
	for (padding, emptied) in [(0, false), (8192, false), (0, true)] {
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

		let retry = match (padding, emptied) {
			| (_, true) => json!({"devices": [], "auth": auth}),
			| (0, _) => json!({"auth": auth}),
			| _ => json!({"devices": [target], "auth": auth}),
		};

		let response = client
			.post(&url)
			.bearer_auth(token)
			.json(&retry)
			.send()
			.await?;

		assert_eq!(response.status(), StatusCode::OK);

		let exists = services
			.users
			.device_exists(&user_id, &target)
			.await;

		assert_eq!(exists, emptied);

		assert!(
			services
				.uiaa
				.get_uiaa_request(&user_id, Some(&sender), session)
				.is_none()
		);
	}

	Ok(())
}

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise_linked(services: &Services, base: &str) -> Result {
	let password = json!({"stages": ["m.login.password"]});
	let sso = json!({"stages": ["m.login.sso"]});

	// Session ids sort in list order, so `ordered` stores beta's session first.
	for (localpart, credential, tag, linked, bound) in [
		("brand", PASSWORD_SENTINEL, None, &["beta-brand"][..], Some("beta")),
		("default", PASSWORD_SENTINEL, None, &["alpha", "gamma"][..], Some("gamma")),
		("ordered", PASSWORD_SENTINEL, None, &["beta", "alpha"][..], Some("alpha")),
		("tagged", PASSWORD_SENTINEL, Some("alpha"), &["gamma"][..], Some("alpha")),
		("unlinked", PASSWORD_SENTINEL, None, &[][..], None),
		("unconfigured", PASSWORD_SENTINEL, None, &["gone"][..], None),
		("password", "test-password", None, &["beta"][..], Some("beta")),
	] {
		let user_id = UserId::parse_with_server_name(localpart, services.globals.server_name())?;

		services
			.users
			.create(&user_id, Some(credential), Some("sso"))
			.await?;

		for (index, &idp_id) in linked.iter().enumerate() {
			let session = Session {
				idp_id: Some(idp_id.to_owned()),
				sess_id: Some(format!("{localpart}-{index}")),
				user_id: Some(user_id.clone()),
				..Default::default()
			};

			services.oauth.sessions.put(&session).await;
		}

		let challenge = deactivate_challenge(services, base, &user_id, tag).await?;
		let flows: Vec<_> = credential
			.ne(PASSWORD_SENTINEL)
			.then_some(&password)
			.into_iter()
			.chain(bound.map(|_| &sso))
			.collect();

		if challenge["flows"] != json!(flows) {
			return Err!("{localpart}: expected flows {flows:?}, got {}", challenge["flows"]);
		}

		let providers = &challenge["params"]["m.login.sso"]["identity_providers"];
		let expected = bound.map_or(Value::Null, |id| json!([{"id": id}]));

		if *providers != expected {
			return Err!("{localpart}: expected binding {expected}, got {providers}");
		}
	}

	Ok(())
}
