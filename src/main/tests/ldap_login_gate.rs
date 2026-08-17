#![cfg(test)]

use std::{env::temp_dir, fs::remove_dir_all, path::PathBuf, process::id as process_id};

use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result,
	ruma::{UserId, api::error::ErrorKind},
};
use tuwunel_service::{Services, users::PASSWORD_SENTINEL};

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

/// The LDAP login gate keys on account state alone, never on the stored origin
/// or on password ownership.
///
/// A localpart with no local account passes, a password-origin account holding
/// a real local hash passes, and a sentinel account passes. A deactivated
/// account is rejected whatever its origin, and resetting its password to the
/// sentinel reactivates it.
#[test]
fn ldap_login_gate_rejects_only_deactivated_accounts() -> Result {
	let db_path =
		DatabasePath(temp_dir().join(format!("tuwunel-ldap-login-gate-{}", process_id())));

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
	let ghost = UserId::parse_with_server_name("ghost", services.globals.server_name())?;

	services.users.check_ldap_login(&ghost).await?;

	let alice = UserId::parse_with_server_name("alice", services.globals.server_name())?;

	services
		.users
		.create(&alice, Some("correct-horse"), None)
		.await?;

	if !services.users.has_password(&alice).await? {
		return Err!("a real local password must read as password-owned");
	}

	services.users.check_ldap_login(&alice).await?;

	let bob = UserId::parse_with_server_name("bob", services.globals.server_name())?;

	services
		.users
		.create(&bob, Some(PASSWORD_SENTINEL), Some("ldap"))
		.await?;

	if services.users.has_password(&bob).await? {
		return Err!("a sentinel password must not read as password-owned");
	}

	services.users.check_ldap_login(&bob).await?;

	services.users.set_password(&bob, None).await?;

	expect_deactivated(services, &bob).await?;

	services
		.users
		.set_password(&bob, Some(PASSWORD_SENTINEL))
		.await?;

	services.users.check_ldap_login(&bob).await?;

	services.users.set_password(&alice, None).await?;

	expect_deactivated(services, &alice).await
}

async fn expect_deactivated(services: &Services, user_id: &UserId) -> Result {
	match services.users.check_ldap_login(user_id).await {
		| Err(e) if matches!(e.kind(), ErrorKind::UserDeactivated) => Ok(()),
		| Err(e) => Err!("unexpected rejection for a deactivated account: {e}"),
		| Ok(()) => Err!("a deactivated account must not pass the LDAP login gate"),
	}
}
