#![cfg(test)]

use std::{fs::remove_dir_all, path::PathBuf};

use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result,
	ruma::{
		UserId,
		api::client::uiaa::{
			AuthData, AuthFlow, AuthType, MatrixUserIdentifier, Password, UiaaInfo,
			UserIdentifier,
		},
	},
};
use tuwunel_service::{Services, oauth::Session, users::PASSWORD_SENTINEL};

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

#[test]
fn sentinel_password_does_not_bypass_uiaa() -> Result {
	let db_path = DatabasePath(Args::test_database_path("test-uiaa-sentinel"));

	let mut args = Args::default_test(&["fresh", "cleanup"]);

	args.maintenance = true;
	args.option
		.push(format!("database_path={:?}", db_path.0));

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let outcome = exercise(&services).await;
		let shutdown = server.server.shutdown();

		drop(services);

		let run = async_run(&server).await;
		let stop = async_stop(&server).await;

		outcome.and(shutdown).and(run).and(stop)
	});

	drop(runtime);

	result
}

async fn exercise(services: &Services) -> Result {
	let user_id = UserId::parse_with_server_name("alice", services.globals.server_name())?;

	services
		.users
		.create(&user_id, Some(PASSWORD_SENTINEL), Some("sso"))
		.await?;

	let info = UiaaInfo {
		flows: vec![AuthFlow::new(vec![AuthType::Sso])],
		..Default::default()
	};

	if try_wrong_password(services, &user_id, &info).await? {
		return Err!("a password sentinel must not satisfy UIAA without an OAuth session");
	}

	services
		.oauth
		.sessions
		.put(&Session {
			sess_id: Some("oauth-session".to_owned()),
			user_id: Some(user_id.clone()),
			..Default::default()
		})
		.await;

	if try_wrong_password(services, &user_id, &info).await? {
		return Err!("a password sentinel must not satisfy UIAA with an OAuth session");
	}

	Ok(())
}

async fn try_wrong_password(
	services: &Services,
	user_id: &UserId,
	info: &UiaaInfo,
) -> Result<bool> {
	let auth = AuthData::Password(Password::new(
		UserIdentifier::Matrix(MatrixUserIdentifier::new(user_id.localpart().to_owned())),
		"wrong-password".to_owned(),
	));

	services
		.uiaa
		.try_auth(user_id, "ALICEDEVICE".into(), &auth, info)
		.await
		.map(|(worked, _)| worked)
}
