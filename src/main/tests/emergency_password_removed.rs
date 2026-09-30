#![cfg(test)]

use std::{
	env::{current_exe, var},
	fs::remove_dir_all,
	future::ready,
	net::TcpListener,
	path::{Path, PathBuf},
	pin::pin,
	process::Command,
	sync::Arc,
	time::Duration,
};

use axum::{
	Json, Router,
	extract::State,
	routing::{get, post},
};
use axum_server::from_tcp;
use futures::future::select;
use serde_json::{Value as JsonValue, json};
use tokio::{
	sync::{Notify, Semaphore},
	time::timeout,
};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result, err, result::NotFound, ruma::events::GlobalAccountDataEventType, utils::BoolExt,
};
use tuwunel_service::{Services, oauth::Session};

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
const PROVIDER_CLIENT_ID: &str = "emergency-password-test-idp";
const PROVIDER_SESSION_ID: &str = "emergency-password-test-provider-session";
const DISCOVERY_PATH: &str = "/.well-known/openid-configuration";
const REVOCATION_PATH: &str = "/revoke";
const DEADLINE: Duration = Duration::from_secs(10);

struct DatabasePath(PathBuf);

/// Identity provider stand-in holding each revocation open until the test has
/// read the server user's password.
///
/// The password must still stand while a revocation is in flight, or a start
/// interrupted there would not retry the cleanup.
struct Provider {
	issuer: String,

	/// Signalled as each revocation arrives.
	revoking: Notify,

	/// Holds revocations unanswered until the test adds a permit.
	answer: Semaphore,
}

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
/// option and opens a session, and the last start runs without it. The server
/// user's identity provider session must be revoked before its password is
/// cleared, so a start interrupted mid-revocation still finds the password and
/// retries.
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

		if !has_session(services).await? {
			return Err!("a start without the emergency password signed {server_user} out");
		}

		if has_password(services).await? {
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

/// Boots with the emergency password set and opens a session and an identity
/// provider session for the server user.
///
/// This is what an operator recovering admin access does; the provider session
/// stands in for one linked to the server user through SSO.
fn set_phase(database: &Path) -> Result {
	let args = database_args(database, &["fresh"])
		.with_option(format!("emergency_password=\"{EMERGENCY_PASSWORD}\""));

	boot(&args, async |services| {
		let server_user = &services.globals.server_user;

		if !poll_until(DEADLINE, async || has_password(services).await.unwrap_or(false)).await {
			return Err!("the emergency password was never set for {server_user}");
		}

		open_session(services).await?;
		open_provider_session(services).await;

		if !has_session(services).await? {
			return Err!("the session opened for {server_user} was not found");
		}

		Ok(())
	})
}

async fn open_provider_session(services: &Services) {
	let session = Session {
		idp_id: PROVIDER_CLIENT_ID.to_owned().into(),
		sess_id: PROVIDER_SESSION_ID.to_owned().into(),
		access_token: SESSION_TOKEN.to_owned().into(),
		user_id: services.globals.server_user.clone().into(),
		..Default::default()
	};

	services.oauth.sessions.put(&session).await;
}

/// Boots the same database with the option removed and a stand-in identity
/// provider.
///
/// The provider session's revocation must arrive while the password stands,
/// and afterwards the password and the session must both be gone.
fn removed_phase(database: &Path) -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let provider = Arc::new(Provider {
		issuer: format!("http://{}", listener.local_addr()?),
		revoking: Notify::new(),
		answer: Semaphore::new(0),
	});

	let args = database_args(database, &[])
		.with_option(format!("identity_provider.test.client_id=\"{PROVIDER_CLIENT_ID}\""))
		.with_option("identity_provider.test.client_secret=\"test-secret\"")
		.with_option("identity_provider.test.brand=\"test\"")
		.with_option(format!("identity_provider.test.issuer_url=\"{}\"", provider.issuer));

	listener.set_nonblocking(true)?;

	boot(&args, async |services| {
		let serve = serve_provider(listener, Arc::clone(&provider));
		let removal = check_removal(services, &provider);

		select(pin!(serve), pin!(removal))
			.await
			.factor_first()
			.0
	})
}

/// Serves discovery and revocation until the test is done.
///
/// The server stopping first is an error, never a pass.
async fn serve_provider(listener: TcpListener, provider: Arc<Provider>) -> Result {
	let app = Router::new()
		.route(DISCOVERY_PATH, get(discover))
		.route(REVOCATION_PATH, post(revoke))
		.with_state(provider);

	from_tcp(listener)?
		.serve(app.into_make_service())
		.await?;

	Err!("the identity provider stand-in stopped serving")
}

async fn discover(State(provider): State<Arc<Provider>>) -> Json<JsonValue> {
	let revocation = format!("{}{REVOCATION_PATH}", provider.issuer);

	Json(json!({
		"issuer": provider.issuer,
		"revocation_endpoint": revocation,
	}))
}

async fn revoke(State(provider): State<Arc<Provider>>) -> Json<JsonValue> {
	provider.revoking.notify_one();

	// The permit returns on drop, so revocations after the first pass freely.
	drop(provider.answer.acquire().await);

	Json(json!({}))
}

async fn check_removal(services: &Services, provider: &Provider) -> Result {
	let server_user = &services.globals.server_user;

	timeout(DEADLINE, provider.revoking.notified())
		.await
		.map_err(|_| err!("the provider session of {server_user} was never revoked"))?;

	let marked = has_password(services).await.unwrap_or(false);

	provider.answer.add_permits(1);
	if !marked {
		return Err!(
			"{server_user} lost its password before its provider session was revoked, so an \
			 interrupted cleanup would not be retried"
		);
	}

	poll_until(DEADLINE, async || {
		// An unreadable password or session counts as still there.
		!has_password(services).await.unwrap_or(true)
			&& !has_session(services).await.unwrap_or(true)
	})
	.await
	.into_option()
	.ok_or_else(|| {
		err!("removing the emergency password left {server_user} with its password or session")
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

async fn has_password(services: &Services) -> Result<bool> {
	services
		.users
		.has_password(&services.globals.server_user)
		.await
}

async fn has_session(services: &Services) -> Result<bool> {
	services
		.users
		.find_from_token(SESSION_TOKEN)
		.await
		.optional()
		.map(|session| session.is_some())
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
