#![cfg(test)]

use std::sync::Arc;

use tuwunel::{Args, Runtime, Server};
use tuwunel_core::{
	Err, Result, ruma::
		UserId
	,
};
use tuwunel_service::Services;
use tuwunel_service::users::Register;


const PRIMARY: &str = "primary.example.test";
const ALT: &str = "alt.example.test";

fn alt_domain_user_id() -> &'static UserId {
	"@bob:alt.example.test"
		.try_into()
		.expect("valid user id")
}

#[test]
fn alternate_server_names_register_login_message_federate() -> Result {
	let mut args = Args::default_test(&["fresh", "cleanup"]);

	args.option
		.push(format!("server_name=\"{PRIMARY}\""));
	args.option
		.push(format!("alternate_server_names=[\"{ALT}\"]"));

	args.maintenance = true;

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;

	let result: Result = runtime.block_on(async {
		let services = tuwunel::async_start(&server).await?;

		let outcome = run_tests(&services).await;

		server.server.shutdown()?;
		drop(services);
		tuwunel::async_run(&server).await?;
		tuwunel::async_stop(&server).await?;

		outcome
	});

	drop(runtime);
	result
}

async fn run_tests(services: &Arc<Services>) -> Result {
	test_register(services).await?;
	Ok(())
}

/// Test that an alternate-domain user can be registered.
async fn test_register(services: &Arc<Services>) -> Result {
	let alternate_user_id = alt_domain_user_id();

	services
		.users
		.full_register(Register {
			user_id: Some(&alternate_user_id),
			password: Some("alternateuserpassword"),
			is_appservice: false,
			is_guest: false,
			grant_first_user_admin: false,
			..Default::default()
		})
		.await?;

	if !services.users.exists(alternate_user_id).await {
		return Err!("({alternate_user_id}) was not found after registration");
	}

	Ok(())
}
