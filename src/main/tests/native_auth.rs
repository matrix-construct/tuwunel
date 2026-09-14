#![cfg(test)]

use std::fs::remove_dir_all;

use tuwunel::{Args, Runtime, Server};
use tuwunel_core::{Err, Result, ruma::UserId};
use tuwunel_service::{Services, users::Register};

/// The OIDC server (next-gen auth) constructs in native mode with no
/// third-party `identity_provider`, and the login-token tail the native handler
/// relies on round-trips for a freshly registered local account.
#[test]
fn native_oidc_serves_local_accounts() -> Result {
	let db_path = Args::test_database_path("native-auth");

	let args = Args::default_test(&["fresh", "cleanup"])
		.with_database_path(&db_path)
		.with_maintenance()
		.with_option("well_known.client=\"https://localhost\"")
		.with_option("oidc_native_auth=true");

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;

	let result: Result = runtime.block_on(async {
		let services = tuwunel::async_start(&server).await?;

		let outcome = native_round_trip(&services).await;

		server.server.shutdown()?;
		drop(services);

		tuwunel::async_run(&server).await?;
		tuwunel::async_stop(&server).await?;

		outcome
	});

	drop(runtime);

	remove_dir_all(&db_path).ok();

	result
}

async fn native_round_trip(services: &Services) -> Result {
	// The dependency on identity_provider is broken: the OIDC server is up.
	let oidc = services.oauth.get_server()?;

	let issuer = oidc.issuer_url()?;
	if !issuer.starts_with("https://localhost") {
		return Err!("unexpected issuer: {issuer}");
	}

	let user_id = UserId::parse_with_server_name("nativealice", services.globals.server_name())?;
	services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password: Some("a-strong-test-password"),
			..Default::default()
		})
		.await?;

	// The native submit handler authenticates, mints a login token, and lets
	// _complete consume it; exercise that token tail directly.
	let token = "native-auth-test-login-token";
	let _expires_in = services.users.create_login_token(&user_id, token);
	let resolved = services
		.users
		.find_from_login_token(token)
		.await?;

	if resolved != user_id {
		return Err!("login token resolved to the wrong user: {resolved}");
	}

	Ok(())
}
