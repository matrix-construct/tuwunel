use std::{
	env::{current_exe, temp_dir, var_os},
	fs::remove_dir_all,
	path::{Path, PathBuf},
	sync::Arc,
	thread::current as current_thread,
};

use tokio::{process::Command, runtime::Handle};
use tracing::subscriber::NoSubscriber;
use tuwunel_core::{
	Err, Result, Server,
	config::{Config, Figment, Sources},
	err,
	log::{LogLevelReloadHandles, Logging, capture::State},
	metrics::Metrics,
	utils::random_string,
};

use crate::Services;

pub(crate) struct Fixture {
	pub services: Arc<Services>,
}

struct DatabasePath(PathBuf);

const CHILD_TEST_ENV: &str = "SERVICE_TEST_CHILD";
const CHILD_DATABASE_ENV: &str = "SERVICE_TEST_DATABASE";

/// Runs the calling test in a child process and builds its services there.
///
/// The parent returns `None` after checking the child's result. Service references
/// form cycles, so only process exit releases their database handles.
pub(crate) async fn fixture(config: Figment) -> Result<Option<Fixture>> {
	let thread = current_thread();
	let name = thread
		.name()
		.ok_or_else(|| err!("service fixture requires a named test thread"))?;

	match (var_os(CHILD_TEST_ENV), var_os(CHILD_DATABASE_ENV)) {
		| (None, None) => {
			run_test(name).await?;
			Ok(None)
		},
		| (Some(child), Some(path)) if child == name => {
			let fixture = build(config, Path::new(&path)).await?;

			Ok(Some(fixture))
		},
		| _ => Err!("service fixture child markers do not match {name}"),
	}
}

async fn run_test(name: &str) -> Result {
	let path = temp_dir().join("tuwunel").join(random_string(32));
	let status = Command::new(current_exe()?)
		.arg(concat!("-", "-exact"))
		.arg(name)
		.env(CHILD_TEST_ENV, name)
		.env(CHILD_DATABASE_ENV, &path)
		.kill_on_drop(true)
		.status()
		.await?;

	// Arm cleanup only after reaping; cancellation must not remove an open database.
	let path = DatabasePath(path);

	if !status.success() {
		return Err!("isolated service test {name} failed with {status}");
	}

	if !path.0.is_dir() {
		return Err!("isolated service test {name} did not create its fixture");
	}

	Ok(())
}

async fn build(config: Figment, path: &Path) -> Result<Fixture> {
	let raw = Figment::new()
		.merge(("server_name", "localhost"))
		.merge(config)
		.merge(("database_path", path.to_string_lossy().as_ref()));

	let config = Config::new(&raw)?;
	let runtime = Handle::current();
	let logging = Logging {
		subscriber: Arc::new(NoSubscriber::new()),
		reload: LogLevelReloadHandles::default(),
		capture: Arc::new(State::new()),
	};

	let metrics = Metrics::new(Some(&runtime));
	let server =
		Arc::new(Server::new(config, Sources::default(), Some(&runtime), logging, metrics));

	let services = Services::build(server).await?;

	Ok(Fixture { services })
}

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}
