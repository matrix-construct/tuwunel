#![cfg(test)]

//! The documented offline integrity run walks every column family.
//!
//! A first child boots a fresh database, the parent then adds a damaged family
//! this build does not describe, and later children reopen it read-only: one
//! asks to schedule a repair, and one starts from the operator's command line
//! with `--read-only` and `--execute`. Each boot is its own process because a
//! booted engine holds the database lock until exit.

use std::{
	env::{current_exe, var},
	fs::{OpenOptions, remove_dir_all},
	io::{Seek, SeekFrom, Write},
	path::{Path, PathBuf},
	process::Command,
};

use clap::Parser;
use rust_rocksdb::{DB, Error as EngineError, Options};
use tuwunel::{Args, Runtime, Server, async_exec};
use tuwunel_core::{Error, Result, err};
use tuwunel_service::Services;

use self::{admin::refused, fixture::boot_at, reply::replied};

#[expect(dead_code)] // Only refusals go through the shared admin assertions.
mod admin;
#[expect(
	dead_code,
	reason = "Only listener readiness is shared with the client API harness."
)]
mod client;
#[expect(dead_code)] // Every phase reopens one database, so only `boot_at` is used.
mod fixture;
#[path = "admin/reply.rs"]
mod reply;

const CHILD_DATABASE_ENV: &str = "INTEGRITY_OFFLINE_TEST_DATABASE";
const CHILD_PHASE_ENV: &str = "INTEGRITY_OFFLINE_TEST_PHASE";
const FOREIGN: &str = "foreign_leftover";
const FOREIGN_ROWS: u32 = 4096;
const READ_ONLY: [&str; 2] = ["rocksdb_read_only=true", "database_migrations=false"];
const REPAIR: &str = "integrity repair relation-index --yes-i-know-what-im-doing";

// The offline run as the maintenance guide gives it, less the configuration.
const OFFLINE_RUN: [&str; 8] = [
	"tuwunel",
	"--read-only",
	"-O",
	"database_migrations=false",
	"--execute",
	"integrity check",
	"--execute",
	"server shutdown",
];

// Where the damage lands, inside the first data block of the family's table.
const DAMAGE_OFFSET: u64 = 64;
const DAMAGE: [u8; 32] = [0xA5; 32];

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

/// The offline run walks a foreign family and runs the quiescent checks.
///
/// Its printed findings show the damage `columns` met in a family no build
/// describes and `unused-columns` listing that family, with a quiescent check
/// run rather than skipped, before the run shuts itself down. Scheduling a
/// repair on the read-only database is refused.
#[test]
fn integrity_check_offline_walks_a_foreign_family() -> Result {
	if let Ok(phase) = var(CHILD_PHASE_ENV) {
		let database = var(CHILD_DATABASE_ENV).expect("integrity offline child database is set");
		let database = PathBuf::from(database);

		return match phase.as_str() {
			| "build" => boot_at(&database, &["fresh"], [], build),
			| "refuse" => boot_at(&database, &[], READ_ONLY, refuse),
			| "offline" => offline(&database),
			| _ => panic!("unknown integrity offline phase: {phase}"),
		};
	}

	let database = DatabasePath(Args::test_database_path("integrity-offline"));

	run_child(&database.0, "build")?;
	seed_damaged_family(&database.0)?;
	run_child(&database.0, "refuse")?;

	let output = run_child(&database.0, "offline")?;

	assert!(output.contains(&format!("column {FOREIGN} failed a full read")), "{output}");
	assert!(output.contains(&format!("column {FOREIGN} is unused")), "{output}");
	assert!(output.contains("relation-index: clean"), "{output}");
	assert!(!output.contains("Skipped on a writable database"), "{output}");
	assert!(output.contains("Shutting down server"), "{output}");

	Ok(())
}

async fn build(services: &Services, _: &str) -> Result {
	let report = replied(services, "integrity check schema").await?;

	assert!(report.contains("schema: clean"), "{report}");

	Ok(())
}

async fn refuse(services: &Services, _: &str) -> Result {
	assert!(services.db.is_read_only(), "the refuse phase must open the database read-only");

	refused(services, REPAIR, "scheduling a repair needs a writable database").await
}

/// Starts the server from the offline run's command line, as an operator would.
///
/// It returns once the run's last command has shut the server down.
fn offline(database: &Path) -> Result {
	let args = Args::try_parse_from(OFFLINE_RUN)
		.map_err(|error| err!("offline run arguments: {error}"))?
		.with_option("server_name=\"localhost\"")
		.with_database_path(database);

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let outcome = runtime.block_on(async_exec(&server));

	drop(runtime);

	outcome
}

/// Runs one phase in a child process, returning everything the child printed.
///
/// The child's harness capture is off, so the findings the offline run prints
/// reach the parent.
fn run_child(database: &Path, phase: &str) -> Result<String> {
	let output = Command::new(current_exe()?)
		.env(CHILD_DATABASE_ENV, database)
		.env(CHILD_PHASE_ENV, phase)
		.env("RUST_TEST_NOCAPTURE", "1")
		.output()?;

	let stdout = String::from_utf8_lossy(&output.stdout);
	let stderr = String::from_utf8_lossy(&output.stderr);

	assert!(
		output.status.success(),
		"integrity offline {phase} child failed with {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
		output.status,
	);

	Ok(format!("{stdout}{stderr}"))
}

/// Adds a family this build does not describe, then damages its table file.
///
/// The family is flushed to one table and the server is not running, so the
/// damage sits on disk for the read-only boot to find.
fn seed_damaged_family(database: &Path) -> Result {
	let opts = Options::default();
	let columns = DB::list_cf(&opts, database).map_err(storage_error)?;
	let db = DB::open_cf(&opts, database, &columns).map_err(storage_error)?;

	db.create_cf(FOREIGN, &opts)
		.map_err(storage_error)?;

	let family = db
		.cf_handle(FOREIGN)
		.ok_or_else(|| err!("column {FOREIGN} missing from the test database"))?;

	for row in 0..FOREIGN_ROWS {
		db.put_cf(&family, row.to_be_bytes(), [b'x'; 64])
			.map_err(storage_error)?;
	}

	db.flush_cf(&family).map_err(storage_error)?;

	let table = db
		.live_files()
		.map_err(storage_error)?
		.into_iter()
		.find(|file| file.column_family_name == FOREIGN)
		.map(|file| Path::new(&file.directory).join(file.name.trim_start_matches('/')))
		.ok_or_else(|| err!("column {FOREIGN} has no table file"))?;

	drop(family);
	drop(db);

	let mut file = OpenOptions::new().write(true).open(table)?; // Write takes &mut self

	file.seek(SeekFrom::Start(DAMAGE_OFFSET))?;
	file.write_all(&DAMAGE)?;
	file.sync_all().map_err(Into::into)
}

fn storage_error(error: EngineError) -> Error {
	let error = error.into_string();

	err!("storage engine: {error}")
}
