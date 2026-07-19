#![cfg(test)]

use std::{fs::remove_dir_all, process::id as process_id};

use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{Err, Result, err, ruma::UserId, utils::BoolExt};
use tuwunel_service::{Services, oauth::server::DeviceGrantPoll};

#[test]
fn native_device_grant_approves_without_idp() -> Result {
	let db_path = format!("/tmp/tuwunel-test-native-device-auth-{}", process_id());

	let mut args = Args::default_test(&["fresh", "cleanup"]);

	args.maintenance = true;
	args.option.extend([
		format!("database_path=\"{db_path}\""),
		"well_known.client=\"https://localhost\"".to_owned(),
		"oidc_native_auth=true".to_owned(),
	]);

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

	oidc.approve_device_grant(&grant.user_code, user_id.clone(), None)
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
	})
}
