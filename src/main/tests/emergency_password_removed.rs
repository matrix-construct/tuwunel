#![cfg(test)]

use std::{
	env::{current_exe, var},
	fs::remove_dir_all,
	future::ready,
	path::{Path, PathBuf},
	process::Command,
	sync::Arc,
	time::Duration,
};

use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{Err, Result, result::NotFound, ruma::events::GlobalAccountDataEventType};
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
/// Separate child processes boot one database in turn. A server that never
/// set the option opens a session for the server user and restarts twice,
/// leaving the session and its push rules alone; a fresh start then sets the
/// option and opens a session, and the last start runs without it.
#[test]
fn removing_the_emergency_password_signs_the_server_user_out() -> Result {
	if let Ok(phase) = var(CHILD_PHASE_ENV) {
		let database: PathBuf = var(CHILD_DATABASE_ENV)
			.expect("emergency password child database is configured")
			.into();

		return match phase.as_str() {
			| "never_set" => never_set_phase(&database),
			| "restart" => restart_phase(&database),
			| "untouched" => untouched_phase(&database),
			| "set" => set_phase(&database),
			| "removed" => removed_phase(&database),
			| _ => Err!("unknown emergency password child phase: {phase}"),
		};
	}

	let database = DatabasePath(Args::test_database_path("emergency-password-removed"));

	["never_set", "restart", "untouched", "set", "removed"]
		.into_iter()
		.try_for_each(|phase| run_child(&database.0, phase))
}

/// Boots a fresh database that never set the option and opens a session for
/// the server user.
///
/// A cleanup started by mistake on a later start would sign that session out.
fn never_set_phase(database: &Path) -> Result {
	boot(&database_args(database, &["fresh"]), open_session)
}

/// Boots again without the option and does nothing else.
///
/// A cleanup this start wrongly began runs to the end before the process stops,
/// so the next start sees all of its effect.
fn restart_phase(database: &Path) -> Result {
	boot(&database_args(database, &[]), |_: &Services| ready(Ok(())))
}

/// Boots once more and checks the session and the push rules the never-set
/// start left.
///
/// No start without the option may sign the server user out, give it a
/// password or reset its push rules.
fn untouched_phase(database: &Path) -> Result {
	boot(&database_args(database, &[]), async |services| {
		let server_user = &services.globals.server_user;

		if !has_session(services).await {
			return Err!("a start without the emergency password signed {server_user} out");
		}

		if services.users.has_password(server_user).await? {
			return Err!("a start without the emergency password gave {server_user} a password");
		}

		if has_push_rules(services).await? {
			return Err!(
				"a start without the emergency password reset the push rules of {server_user}"
			);
		}

		Ok(())
	})
}

/// Boots with the emergency password set and opens a session for the server
/// user.
///
/// This is what an operator recovering admin access does.
fn set_phase(database: &Path) -> Result {
	let args = database_args(database, &["fresh"])
		.with_option(format!("emergency_password=\"{EMERGENCY_PASSWORD}\""));

	boot(&args, async |services| {
		let server_user = &services.globals.server_user;

		if !poll_until(DEADLINE, async || has_password(services).await).await {
			return Err!("the emergency password was never set for {server_user}");
		}

		open_session(services).await?;

		if !has_session(services).await {
			return Err!("the session opened for {server_user} was not found");
		}

		Ok(())
	})
}

/// Boots the same database with the option removed.
///
/// The password and the session must both be gone.
fn removed_phase(database: &Path) -> Result {
	boot(&database_args(database, &[]), async |services| {
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

fn database_args(database: &Path, test: &[&str]) -> Args {
	Args::default_test(test).with_option(format!("database_path={database:?}"))
}

async fn open_session(services: &Services) -> Result {
	services
		.users
		.create_device(
			&services.globals.server_user,
			None,
			(Some(SESSION_TOKEN), None),
			None,
			None,
			None,
		)
		.await
		.map(drop)
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

// A failed read is an error, not an absence, so it cannot pass for untouched rules.
async fn has_push_rules(services: &Services) -> Result<bool> {
	let kind = GlobalAccountDataEventType::PushRules.to_string();

	services
		.account_data
		.get_raw(None, &services.globals.server_user, &kind)
		.await
		.optional()
		.map(|raw| raw.is_some())
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
