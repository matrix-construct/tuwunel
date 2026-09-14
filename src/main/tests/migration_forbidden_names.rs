#![cfg(test)]

use std::{
	env::{current_exe, var},
	fs::remove_dir_all,
	path::{Path, PathBuf},
	process::Command,
	sync::{
		Arc,
		atomic::{AtomicUsize, Ordering::SeqCst},
	},
	time::Duration,
};

use tokio::time::timeout;
use tuwunel::{Args, Runtime, Server, async_exec, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result,
	log::{Capture, capture::Data},
	ruma::{RoomAliasId, RoomId, UserId},
};
use tuwunel_service::{Services, users::PASSWORD_SENTINEL};

const CHILD_DATABASE_ENV: &str = "MIGRATION_SCAN_TEST_DATABASE";
const CHILD_PHASE_ENV: &str = "MIGRATION_SCAN_TEST_PHASE";
const RECORD_COUNT: usize = 4_096;

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { drop(remove_dir_all(&self.0)); }
}

/// A synchronous stop on the first matching warning leaves 4,095 matching
/// records unvisited. Separate child processes exercise both lazy scan layers.
#[test]
fn forbidden_name_scans_stop_within_one_item() -> Result {
	if let Ok(phase) = var(CHILD_PHASE_ENV) {
		let database = PathBuf::from(
			var(CHILD_DATABASE_ENV).expect("migration scan child database is configured"),
		);

		return match phase.as_str() {
			| "seed" => seed_phase(&database),
			| "users" => scan_phase(&database, "users"),
			| "aliases" => scan_phase(&database, "aliases"),
			| _ => Err!("unknown migration scan child phase: {phase}"),
		};
	}

	let database = DatabasePath(Args::test_database_path("migration-scan"));

	for phase in ["seed", "users", "aliases"] {
		run_child(&database.0, phase)?;
	}

	Ok(())
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
			"migration scan {phase} child failed with {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
			output.status,
		);
	}

	Ok(())
}

fn seed_phase(database: &Path) -> Result {
	let args = Args::default_test(&["fresh"]).with_option(format!("database_path={database:?}"));

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let outcome = seed_records(&services).await;
		let shutdown = server.server.shutdown();

		drop(services);

		let run = async_run(&server).await;
		let stop = async_stop(&server).await;

		outcome.and(shutdown).and(run).and(stop)
	});

	drop(runtime);

	result
}

async fn seed_records(services: &Services) -> Result {
	let server_name = services.globals.server_name();

	for index in 0..RECORD_COUNT {
		let user_id = UserId::parse_with_server_name(format!("scanuser{index:04}"), server_name)?;

		services
			.users
			.create(&user_id, Some(PASSWORD_SENTINEL), Some("migration scan"))
			.await?;
	}

	let room_id = RoomId::parse(format!("!forbidden-scan:{server_name}"))?;

	services
		.short
		.get_or_create_shortroomid(&room_id)
		.await;

	for index in 0..RECORD_COUNT {
		let alias = RoomAliasId::parse(format!("#scanalias{index:04}:{server_name}"))?;

		services.alias.set_alias(&alias, &room_id)?;
	}

	Ok(())
}

fn scan_phase(database: &Path, phase: &str) -> Result {
	let (option, warning) = match phase {
		| "users" =>
			("forbidden_usernames=[\"^scanuser\"]", "matches forbidden username patterns"),
		| "aliases" => (
			"forbidden_alias_names=[\"^scanalias\"]",
			"matches the following forbidden alias name patterns",
		),
		| _ => return Err!("unknown migration scan phase: {phase}"),
	};

	let args = Args::default_test(&[])
		.with_option(format!("database_path={database:?}"))
		.with_option(option);

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let matches = Arc::new(AtomicUsize::new(0));
	let stopped = Arc::new(AtomicUsize::new(0));
	let callback_matches = matches.clone();
	let callback_stopped = stopped.clone();
	let callback_server = server.server.clone();
	let capture = Capture::new(
		&server.server.log.capture,
		Some(|data: Data<'_>| data.mod_name() == "tuwunel_service::migrations"),
		move |data: Data<'_>| {
			let message = data.message();

			if message.contains(warning) && callback_matches.fetch_add(1, SeqCst) == 0 {
				callback_server
					.shutdown()
					.expect("migration scan capture failed to request shutdown");
			}

			if message.starts_with("Stopped during database migrations") {
				callback_stopped.fetch_add(1, SeqCst);
			}
		},
	);

	let _capture_guard = capture.start();
	let result = match runtime
		.block_on(async { timeout(Duration::from_secs(30), async_exec(&server)).await })
	{
		| Ok(result) => result,
		| Err(error) => return Err!("migration scan {phase} did not stop in time: {error}"),
	};

	drop(runtime);

	assert!(result.is_ok(), "migration scan {phase} failed: {result:?}");
	assert!(server.server.is_stopping(), "migration scan did not request shutdown");
	assert_eq!(matches.load(SeqCst), 1, "migration scan processed more than one item");
	assert_eq!(stopped.load(SeqCst), 1, "migration stop warning count changed");

	Ok(())
}
