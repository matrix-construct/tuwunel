#![cfg(test)]

use std::fs::remove_dir_all;

use tokio::join;
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{Err, Result, err, ruma::UserId, utils::BoolExt};
use tuwunel_service::{Services, oauth::server::DeviceGrantPoll, users::LoginProviderId};

#[test]
fn native_device_grant_approves_without_idp() -> Result {
	let db_path = Args::test_database_path("native-device-auth");

	let args = Args::default_test(&["fresh", "cleanup"])
		.with_database_path(&db_path)
		.with_maintenance()
		.with_option("well_known.client=\"https://localhost\"")
		.with_option("oidc_native_auth=true");

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;

	let result: Result = runtime.block_on(async {
		let services = async_start(&server).await?;

		let outcome = round_trip(&services).await;

		server.server.shutdown()?;
		drop(services);

		async_run(&server).await?;
		async_stop(&server).await?;

		outcome
	});

	drop(runtime);

	remove_dir_all(&db_path).ok();

	result
}

async fn round_trip(services: &Services) -> Result {
	BoolExt::ok_or_else(
		services
			.oauth
			.providers
			.get_default_id()
			.is_none(),
		|| err!("native device test unexpectedly configured an identity provider"),
	)?;

	let oidc = services.oauth.get_server()?;
	let client_id = "native-device-client";
	let grant = oidc.create_device_grant(client_id, "openid");
	let user_id = UserId::parse_with_server_name("nativealice", services.globals.server_name())?;
	let token = "native-device-login-token";
	let _expires_in = services.users.create_login_token(&user_id, token);
	let login = services
		.users
		.peek_login_token_with_provider(token)
		.await?;

	oidc.verify_device_grant(&grant.user_code, &login.user_id, login.idp_id.as_deref())
		.await?;

	let other_user = UserId::parse_with_server_name("nativebob", services.globals.server_name())?;
	let cross_user_denied = oidc
		.deny_device_grant(&grant.user_code, other_user, None)
		.await
		.is_err();

	BoolExt::ok_or_else(cross_user_denied, || {
		err!("device grant accepted consent from another user")
	})?;

	let altered_provider_rejected = oidc
		.approve_device_grant(
			&grant.user_code,
			login.user_id.clone(),
			Some("tampered-provider".into()),
		)
		.await
		.is_err();

	BoolExt::ok_or_else(altered_provider_rejected, || {
		err!("device grant accepted tampered provider attribution")
	})?;

	let login = services
		.users
		.find_login_token_with_provider(token)
		.await?;

	oidc.approve_device_grant(
		&grant.user_code,
		login.user_id,
		login.idp_id.map(LoginProviderId::into_string),
	)
	.await?;

	let DeviceGrantPoll::Approved(approved) = oidc
		.poll_device_grant(&grant.device_code, client_id)
		.await?
	else {
		return Err!("native device grant was not approved");
	};

	BoolExt::ok_or_else(approved.user_id == user_id, || {
		err!("native device grant resolved to the wrong user")
	})?;

	BoolExt::ok_or_else(approved.idp_id.is_none(), || {
		err!("native device grant unexpectedly carried an identity provider")
	})?;

	let grant = oidc.create_device_grant(client_id, "openid");
	let token = "sso-device-login-token";
	let provider = "selected-provider";
	let _expires_in =
		services
			.users
			.create_login_token_with_provider(&user_id, token, Some(provider));

	let login = services
		.users
		.peek_login_token_with_provider(token)
		.await?;

	BoolExt::ok_or_else(login.idp_id.as_deref() == Some(provider), || {
		err!("SSO login token lost its provider")
	})?;

	oidc.verify_device_grant(&grant.user_code, &login.user_id, login.idp_id.as_deref())
		.await?;

	let login = services
		.users
		.find_login_token_with_provider(token)
		.await?;

	oidc.approve_device_grant(
		&grant.user_code,
		login.user_id,
		login.idp_id.map(LoginProviderId::into_string),
	)
	.await?;

	let DeviceGrantPoll::Approved(approved) = oidc
		.poll_device_grant(&grant.device_code, client_id)
		.await?
	else {
		return Err!("SSO device grant was not approved");
	};

	BoolExt::ok_or_else(approved.idp_id.as_deref() == Some(provider), || {
		err!("SSO device grant carried the wrong identity provider")
	})?;

	let token_rejected = services
		.users
		.find_login_token_with_provider(token)
		.await
		.is_err();

	BoolExt::ok_or_else(token_rejected, || err!("SSO login token was reusable"))?;

	let token = "concurrent-device-login-token";
	let _expires_in = services.users.create_login_token(&user_id, token);
	let first = services
		.users
		.find_login_token_with_provider(token);

	let second = services
		.users
		.find_login_token_with_provider(token);

	let (first, second) = join!(first, second);

	BoolExt::ok_or_else(first.is_ok() != second.is_ok(), || {
		err!("concurrent login token redemption was not single-use")
	})
}
