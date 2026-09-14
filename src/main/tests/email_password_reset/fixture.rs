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
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{Err, Result, err};
use tuwunel_service::Services;

use super::{
	RestartState,
	scenarios::{JWT_SECRET, first_phase, second_phase},
};

struct TestPaths {
	database: PathBuf,
	state: PathBuf,
}

#[derive(Clone, Copy)]
struct ServerConfig {
	smtp_port: u16,
}

const TEST_DATABASE_ENV: &str = "EMAIL_PASSWORD_RESET_DATABASE";
const TEST_PHASE_ENV: &str = "EMAIL_PASSWORD_RESET_PHASE";
const TEST_STATE_ENV: &str = "EMAIL_PASSWORD_RESET_STATE";

impl Drop for TestPaths {
	fn drop(&mut self) {
		remove_dir_all(&self.database).ok();
		remove_file(&self.state).ok();
	}
}

pub(super) fn run() -> Result {
	match var(TEST_PHASE_ENV).ok().as_deref() {
		| Some("prepare") => prepare_restart_state(),
		| Some("redeem") => redeem_after_restart(),
		| _ => run_restart_pair(),
	}
}

fn run_restart_pair() -> Result {
	let database = Args::test_database_path("email-password-reset");
	let state = database.with_extension("json");
	let paths = TestPaths { database, state };

	let executable = current_exe()?;

	for phase in ["prepare", "redeem"] {
		let status = Command::new(&executable)
			.env(TEST_PHASE_ENV, phase)
			.env(TEST_DATABASE_ENV, &paths.database)
			.env(TEST_STATE_ENV, &paths.state)
			.status()?;

		if !status.success() {
			return Err!("password-reset {phase} child failed with {status}");
		}
	}

	Ok(())
}

fn prepare_restart_state() -> Result {
	let smtp_listener = TcpListener::bind(("127.0.0.1", 0))?;
	let smtp_port = smtp_listener.local_addr()?.port();
	let database = child_path(TEST_DATABASE_ENV)?;
	let state_path = child_path(TEST_STATE_ENV)?;
	let config = ServerConfig { smtp_port };

	let state = run_server(&database, &["fresh"], config, move |services, client, base| {
		first_phase(services, client, base, smtp_listener)
	})?;

	write(state_path, serde_json::to_vec(&state)?)?;

	Ok(())
}

fn redeem_after_restart() -> Result {
	let smtp_listener = TcpListener::bind(("127.0.0.1", 0))?;
	let smtp_port = smtp_listener.local_addr()?.port();
	let database = child_path(TEST_DATABASE_ENV)?;
	let state_path = child_path(TEST_STATE_ENV)?;
	let state = serde_json::from_slice::<RestartState>(&read(state_path)?)?;
	let config = ServerConfig { smtp_port };

	run_server(&database, &[], config, move |services, client, base| {
		second_phase(services, client, base, state)
	})
}

fn child_path(name: &str) -> Result<PathBuf> {
	var(name)
		.map(PathBuf::from)
		.map_err(|error| err!("missing child-process path {name}: {error}"))
}

fn run_server<T, F, Fut>(
	db_path: &Path,
	test_modes: &[&str],
	config: ServerConfig,
	exercise: F,
) -> Result<T>
where
	F: FnOnce(Arc<Services>, Client, String) -> Fut,
	Fut: Future<Output = Result<T>>,
{
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let mut args = Args::default_test(test_modes);

	args.option.extend([
		format!("database_path={db_path:?}"),
		"address=[\"127.0.0.1\"]".to_owned(),
		format!("port={port}"),
		"listening=true".to_owned(),
		format!("smtp.connection_uri=\"smtp://127.0.0.1:{}\"", config.smtp_port),
		"smtp.sender=\"noreply@example.org\"".to_owned(),
		format!("well_known.client=\"http://127.0.0.1:{port}\""),
		"jwt.enable=true".to_owned(),
		format!("jwt.key=\"{JWT_SECRET}\""),
		"jwt.format=\"HMAC\"".to_owned(),
		"jwt.algorithm=\"HS256\"".to_owned(),
		"jwt.register_user=false".to_owned(),
	]);

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");
		let client = Client::builder()
			.pool_max_idle_per_host(0)
			.build()?;

		drop(listener);

		let exercise = async {
			let outcome = exercise(services.clone(), client, base).await;
			let shutdown = server.server.shutdown();

			outcome.and_then(|value| shutdown.map(|()| value))
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
