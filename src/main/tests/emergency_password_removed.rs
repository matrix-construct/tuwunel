#![cfg(test)]

use std::{
	env::{current_exe, var},
	fs::remove_dir_all,
	path::{Path, PathBuf},
	process::Command,
	sync::Arc,
	time::Duration,
};

use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{Err, Result};
use tuwunel_service::Services;

use self::client::poll_until;

#[expect(
	dead_code,
	reason = "Only polling is shared with the client API harness."
)]
mod client;

const CHILD_DATABASE_ENV: &str = "EMERGENCY_PASSWORD_TEST_DATABASE";
const CHILD_PHASE_ENV: &str = "EMERGENCY_PASSWORD_TEST_PHASE";
const EMERGENCY_PASSWORD: &str = "emergency-password-test-secret";
const SESSION_TOKEN: &str = "emergency-password-test-access-token";
const DEADLINE: Duration = Duration::from_secs(10);

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

/// Removing `emergency_password` undoes what setting it did: the server
/// user's password is cleared and the sessions opened with it are signed out,
/// as `docs/authentication/legacy.md` promises.
///
/// Separate child processes boot the one database twice: first with the
/// option set and a session opened for the server user, then without it.
#[test]
fn removing_the_emergency_password_signs_the_server_user_out() -> Result {
	if let Ok(phase) = var(CHILD_PHASE_ENV) {
		let database: PathBuf = var(CHILD_DATABASE_ENV)
			.expect("emergency password child database is configured")
			.into();

		return match phase.as_str() {
			| "set" => set_phase(&database),
			| "removed" => removed_phase(&database),
			| _ => Err!("unknown emergency password child phase: {phase}"),
		};
	}

	let database = DatabasePath(Args::test_database_path("emergency-password-removed"));

	for phase in ["set", "removed"] {
		run_child(&database.0, phase)?;
	}

	Ok(())
}

/// Boots with the emergency password set and opens a session for the server
/// user, as an operator recovering admin access does.
fn set_phase(database: &Path) -> Result {
	let args = Args::default_test(&["fresh"])
		.with_option(format!("database_path={database:?}"))
		.with_option(format!("emergency_password=\"{EMERGENCY_PASSWORD}\""));

	boot(&args, async |services| {
		let server_user = &services.globals.server_user;

		if !poll_until(DEADLINE, async || has_password(services).await).await {
			return Err!("the emergency password was never set for {server_user}");
		}

		services
			.users
			.create_device(server_user, None, (Some(SESSION_TOKEN), None), None, None, None)
			.await?;

		if !has_session(services).await {
			return Err!("the session opened for {server_user} was not found");
		}

		Ok(())
	})
}

/// Boots the same database with the option removed. The password and the
/// session must both be gone.
fn removed_phase(database: &Path) -> Result {
	let args = Args::default_test(&[]).with_option(format!("database_path={database:?}"));

	boot(&args, async |services| {
		let server_user = &services.globals.server_user;
		let cleared = poll_until(DEADLINE, async || {
			!has_password(services).await && !has_session(services).await
		})
		.await;

		if !cleared {
			return Err!(
				"removing the emergency password left {server_user} with its password or session"
			);
		}

		Ok(())
	})
}

async fn has_password(services: &Services) -> bool {
	services
		.users
		.has_password(&services.globals.server_user)
		.await
		.unwrap_or(false)
}

async fn has_session(services: &Services) -> bool {
	services
		.users
		.find_from_token(SESSION_TOKEN)
		.await
		.is_ok()
}

fn run_child(database: &Path, phase: &str) -> Result {
	let output = Command::new(current_exe()?)
		.env(CHILD_DATABASE_ENV, database)
		.env(CHILD_PHASE_ENV, phase)
		.output()?;

	if !output.status.success() {
		let stdout = String::from_utf8_lossy(&output.stdout);
		let stderr = String::from_utf8_lossy(&output.stderr);

		return Err!(
			"emergency password {phase} child failed with \
			 {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
			output.status,
		);
	}

	Ok(())
}

fn boot<F>(args: &Args, exercise: F) -> Result
where
	F: AsyncFnOnce(&Services) -> Result,
{
	let (runtime, server) = start(args)?;
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

fn start(args: &Args) -> Result<(Runtime, Arc<Server>)> {
	let runtime = Runtime::new(Some(args))?;
	let server = Server::new(Some(args), Some(&runtime))?;

	Ok((runtime, server))
}
