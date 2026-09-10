#![cfg(test)]

use std::{env::var, sync::Arc};

use tuwunel::{Args, Runtime, Server};
use tuwunel_core::{Err, Result};
use tuwunel_service::{Services, oauth::Session};

const TOML_KEY: &str = "test";

const ENV_BRAND: &str = "TUWUNEL_TEST_OAUTH_BRAND";
const ENV_CLIENT_ID: &str = "TUWUNEL_TEST_OAUTH_CLIENT_ID";
const ENV_CLIENT_SECRET: &str = "TUWUNEL_TEST_OAUTH_CLIENT_SECRET";
const ENV_ISSUER_URL: &str = "TUWUNEL_TEST_OAUTH_ISSUER_URL";
const ENV_CALLBACK_URL: &str = "TUWUNEL_TEST_OAUTH_CALLBACK_URL";
const ENV_TOKEN_URL: &str = "TUWUNEL_TEST_OAUTH_TOKEN_URL";
const ENV_USERINFO_URL: &str = "TUWUNEL_TEST_OAUTH_USERINFO_URL";
const ENV_AUTHORIZATION_URL: &str = "TUWUNEL_TEST_OAUTH_AUTHORIZATION_URL";
const ENV_DISCOVERY: &str = "TUWUNEL_TEST_OAUTH_DISCOVERY";
const ENV_CODE: &str = "TUWUNEL_TEST_OAUTH_CODE";
const ENV_CODE_VERIFIER: &str = "TUWUNEL_TEST_OAUTH_CODE_VERIFIER";

#[test]
fn oauth_request_token() -> Result {
	let env_var = |name: &str| var(name).ok().filter(|s| !s.is_empty());

	let (Some(client_id), Some(code), Some(options)) =
		(env_var(ENV_CLIENT_ID), env_var(ENV_CODE), collect_options())
	else {
		eprintln!(
			"oauth_request_token: skipped (set {ENV_BRAND}, {ENV_CLIENT_ID}, {ENV_CODE} to \
			 enable)"
		);

		return Ok(());
	};

	let mut args = Args::default_test(&["fresh", "cleanup"]);

	args.maintenance = true;
	args.option.extend(options);

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;

	let result: Result = runtime.block_on(async {
		let services = tuwunel::async_start(&server).await?;

		let outcome = exchange(&services, &client_id, &code).await;

		server.server.shutdown()?;
		drop(services);

		tuwunel::async_run(&server).await?;
		tuwunel::async_stop(&server).await?;

		outcome
	});

	drop(runtime);
	result
}

async fn exchange(services: &Arc<Services>, client_id: &str, code: &str) -> Result {
	let env_var = |name: &str| var(name).ok().filter(|s| !s.is_empty());

	let provider = services.oauth.providers.get(client_id).await?;

	let session = Session {
		code_verifier: env_var(ENV_CODE_VERIFIER),
		..Default::default()
	};

	let response = services
		.oauth
		.request_token((&provider, &session), code)
		.await?;

	if response.access_token.is_none() {
		return Err!("token response has no access_token");
	}

	Ok(())
}

fn collect_options() -> Option<impl Iterator<Item = String>> {
	let env_var = |name: &str| var(name).ok().filter(|s| !s.is_empty());

	env_var(ENV_BRAND)?;
	env_var(ENV_CLIENT_ID)?;

	let str_field = |field: &str, name: &str| {
		env_var(name).map(|v| {
			let escaped = escape_toml(&v);

			format!("identity_provider.{TOML_KEY}.{field}=\"{escaped}\"")
		})
	};

	let raw_field = |field: &str, name: &str| {
		env_var(name).map(|v| format!("identity_provider.{TOML_KEY}.{field}={v}"))
	};

	[
		str_field("brand", ENV_BRAND),
		str_field("client_id", ENV_CLIENT_ID),
		str_field("client_secret", ENV_CLIENT_SECRET),
		str_field("issuer_url", ENV_ISSUER_URL),
		str_field("callback_url", ENV_CALLBACK_URL),
		str_field("token_url", ENV_TOKEN_URL),
		str_field("userinfo_url", ENV_USERINFO_URL),
		str_field("authorization_url", ENV_AUTHORIZATION_URL),
		raw_field("discovery", ENV_DISCOVERY),
	]
	.into_iter()
	.flatten()
	.into()
}

fn escape_toml(value: &str) -> String { value.replace('\\', "\\\\").replace('"', "\\\"") }
