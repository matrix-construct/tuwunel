use std::{
	env::{current_exe, var},
	fs::{read, remove_dir_all, remove_file, write},
	net::TcpListener,
	path::{Path, PathBuf},
	process::Command,
	sync::Arc,
};

use futures::future::join;
use reqwest::Client;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{Result, err};
use tuwunel_service::Services;

use super::{FIRST_USERNAME, SECOND_USERNAME};

pub(super) const CHILD_DB_ENV: &str = "TUWUNEL_EMAIL_REGISTRATION_CLAIM_DB";
pub(super) const CHILD_PHASE_ENV: &str = "TUWUNEL_EMAIL_REGISTRATION_CLAIM_PHASE";
const CHILD_STATE_ENV: &str = "TUWUNEL_EMAIL_REGISTRATION_CLAIM_STATE";

#[derive(Clone, Copy)]
pub(super) struct RegistrationConfig {
	pub(super) terms: bool,
	pub(super) token: Option<&'static str>,
}

pub(super) struct DatabasePath(pub(super) PathBuf);

pub(super) struct Registration {
	pub(super) username: &'static str,
	pub(super) session: String,
}

pub(super) struct ClaimState {
	pub(super) sid: String,
	pub(super) owner: Registration,
	pub(super) loser: Registration,
}

impl DatabasePath {
	pub(super) fn new(label: &str) -> Self {
		Self(Args::test_database_path(format_args!("email-registration-claim-{label}")))
	}
}

impl Drop for DatabasePath {
	fn drop(&mut self) {
		remove_dir_all(&self.0).ok();
		remove_file(self.0.with_extension("state.json")).ok();
	}
}

pub(super) fn run_server<T, F, Fut>(
	db_path: &Path,
	test_modes: &[&str],
	config: RegistrationConfig,
	exercise: F,
) -> Result<T>
where
	F: FnOnce(Arc<Services>, Client, String) -> Fut,
	Fut: Future<Output = Result<T>>,
{
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let args = server_args(db_path, port, test_modes, config);
	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let client = Client::builder()
			.pool_max_idle_per_host(0)
			.build()?;

		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let exercise = exercise(services.clone(), client, base);
		let exercise = async {
			let outcome = exercise.await;
			let shutdown = server.server.shutdown();

			outcome.and_then(|outcome| shutdown.map(|()| outcome))
		};

		let (run_result, outcome) = join(async_run(&server), exercise).await;

		drop(services);
		async_stop(&server).await?;
		run_result?;

		outcome
	});

	drop(server);
	drop(runtime);

	result
}

fn server_args(
	db_path: &Path,
	port: u16,
	test_modes: &[&str],
	config: RegistrationConfig,
) -> Args {
	let mut args = Args::default_test(test_modes);

	args.option.extend([
		format!("database_path=\"{}\"", db_path.display()),
		"address=[\"127.0.0.1\"]".to_owned(),
		format!("port={port}"),
		"listening=true".to_owned(),
		// The test starts multiple servers, so none may claim global tracing.
		"log_enable=false".to_owned(),
		"well_known.client=\"https://localhost\"".to_owned(),
		"smtp.connection_uri=\"smtp://127.0.0.1:1\"".to_owned(),
		"smtp.sender=\"Tests <test@example.test>\"".to_owned(),
		"smtp.require_email_for_registration=true".to_owned(),
		"allow_registration=true".to_owned(),
		"yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse=true"
			.to_owned(),
	]);

	if config.terms {
		args.option.extend([
			"registration_terms.test.version=\"1\"".to_owned(),
			"registration_terms.test.translations.en.name=\"Test terms\"".to_owned(),
			"registration_terms.test.translations.en.url=\"https://example.test/terms\""
				.to_owned(),
		]);
	}

	if let Some(token) = config.token {
		args.option
			.push(format!("registration_token={token:?}"));
	}

	args
}

pub(super) fn run_child(db_path: &Path, phase: &str, state_path: Option<&Path>) -> Result {
	let mut command = Command::new(current_exe()?);

	command
		.env(CHILD_DB_ENV, db_path)
		.env(CHILD_PHASE_ENV, phase);

	if let Some(state_path) = state_path {
		command.env(CHILD_STATE_ENV, state_path);
	}

	let output = command.output()?;

	if !output.status.success() {
		return Err(err!(
			"email registration claim child {phase} failed with {}\nstdout:\n{}\nstderr:\n{}",
			output.status,
			String::from_utf8_lossy(&output.stdout),
			String::from_utf8_lossy(&output.stderr),
		));
	}

	Ok(())
}

pub(super) fn write_claim_state(claim: &ClaimState) -> Result {
	let state_path = child_state_path()?;
	let state = claim_state_json(claim);

	write(state_path, serde_json::to_vec(&state)?)?;

	Ok(())
}

pub(super) fn read_claim_state() -> Result<ClaimState> {
	let state = serde_json::from_slice(&read(child_state_path()?)?)?;

	claim_state_from_json(&state)
}

fn child_state_path() -> Result<PathBuf> {
	var(CHILD_STATE_ENV)
		.map(PathBuf::from)
		.map_err(|e| err!("child state path is unavailable: {e}"))
}

fn claim_state_json(claim: &ClaimState) -> Value {
	json!({
		"sid": claim.sid,
		"owner": {
			"username": claim.owner.username,
			"session": claim.owner.session,
		},
		"loser": {
			"username": claim.loser.username,
			"session": claim.loser.session,
		},
	})
}

fn claim_state_from_json(value: &Value) -> Result<ClaimState> {
	let sid = json_string(value, "sid")?.to_owned();
	let owner = registration_from_json(
		value
			.get("owner")
			.ok_or_else(|| err!("child claim state omitted owner: {value}"))?,
	)?;

	let loser = registration_from_json(
		value
			.get("loser")
			.ok_or_else(|| err!("child claim state omitted loser: {value}"))?,
	)?;

	Ok(ClaimState { sid, owner, loser })
}

fn registration_from_json(value: &Value) -> Result<Registration> {
	let username = match json_string(value, "username")? {
		| FIRST_USERNAME => FIRST_USERNAME,
		| SECOND_USERNAME => SECOND_USERNAME,
		| username => return Err(err!("unexpected child registration username: {username}")),
	};

	let session = json_string(value, "session")?.to_owned();

	Ok(Registration { username, session })
}

fn json_string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
	value
		.get(field)
		.and_then(Value::as_str)
		.ok_or_else(|| err!("child claim state omitted {field}: {value}"))
}
