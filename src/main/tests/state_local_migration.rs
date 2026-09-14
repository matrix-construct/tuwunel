#![cfg(test)]

use std::{
	env::{current_exe, var},
	fs::remove_dir_all,
	path::{Path, PathBuf},
	process::Command,
};

use tuwunel::{Args, Runtime, Server, async_exec, async_run, async_start, async_stop};
use tuwunel_core::{
	Error, Result,
	result::NotFound,
	ruma::{RoomId, UserId},
};
use tuwunel_database::Deserialized;
use tuwunel_service::{Services, users::PASSWORD_SENTINEL};

const CHILD_DATABASE_ENV: &str = "STATE_LOCAL_MIGRATION_TEST_DATABASE";
const CHILD_PHASE_ENV: &str = "STATE_LOCAL_MIGRATION_TEST_PHASE";
const MARKER: &str = "clear_state_local_error_memos";
const OLD_MEMO: &[u8] = b"$old-state-local-memo";
const NEW_MEMO: &[u8] = b"$new-state-local-memo";
const SENTINEL: &[u8] = b"state_local_authoritative_sentinel";
const AUTHORITATIVE_ROOM: &str = "!state-local-migration:tuwunel.invalid";
const AUTHORITATIVE_STATE_HASH: u64 = 0x5A7E_10CA1;

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

#[test]
fn state_local_memo_migration_is_once_only_and_required() -> Result {
	if let Ok(phase) = var(CHILD_PHASE_ENV) {
		let database = PathBuf::from(
			var(CHILD_DATABASE_ENV).expect("migration child database is configured"),
		);

		return match phase.as_str() {
			| "seed" => seed_phase(&database),
			| "migrate" => migrate_phase(&database),
			| "restart" => restart_phase(&database),
			| "reject" => reject_phase(&database),
			| "restore" => restore_phase(&database),
			| _ => panic!("unknown state-local migration phase: {phase}"),
		};
	}

	let database = DatabasePath(Args::test_database_path("state-local-migration"));

	for phase in ["seed", "migrate", "restart", "reject", "restore"] {
		run_child(&database.0, phase)?;
	}

	Ok(())
}

fn run_child(database: &Path, phase: &str) -> Result {
	let output = Command::new(current_exe()?)
		.env(CHILD_DATABASE_ENV, database)
		.env(CHILD_PHASE_ENV, phase)
		.output()?;

	assert!(
		output.status.success(),
		"state-local migration {phase} child failed with {}\nstdout:\n{}\nstderr:\n{}",
		output.status,
		String::from_utf8_lossy(&output.stdout),
		String::from_utf8_lossy(&output.stderr),
	);

	Ok(())
}

fn seed_phase(database: &Path) -> Result {
	let args = Args::default_test(&["fresh"]).with_option(format!("database_path={database:?}"));

	with_started_server(&args, async |services| {
		services.db["global"].exists(MARKER).await?;

		let user_id =
			UserId::parse_with_server_name("migration", services.globals.server_name())?;

		services
			.users
			.create(&user_id, Some(PASSWORD_SENTINEL), Some("migration test"))
			.await?;

		services.db["eventid_resolvedstate"].insert(OLD_MEMO, b"old");
		services.db["global"].insert(SENTINEL, b"kept");

		let room_id = RoomId::parse(AUTHORITATIVE_ROOM)?;

		services.db["roomid_shortstatehash"]
			.raw_aput::<{ size_of::<u64>() }, _, _>(room_id.as_bytes(), AUTHORITATIVE_STATE_HASH);
		services.db["global"].remove(MARKER);

		Ok(())
	})
}

fn migrate_phase(database: &Path) -> Result {
	let args = Args::default_test(&[]).with_option(format!("database_path={database:?}"));

	with_started_server(&args, async |services| {
		services.db["global"].exists(MARKER).await?;
		assert!(
			services.db["eventid_resolvedstate"]
				.exists(OLD_MEMO)
				.await
				.is_not_found()
		);

		services.db["global"].exists(SENTINEL).await?;

		assert_authoritative_state(services).await?;

		services.db["eventid_resolvedstate"].insert(NEW_MEMO, b"new");

		Ok(())
	})
}

fn restart_phase(database: &Path) -> Result {
	let args = Args::default_test(&[])
		.with_option(format!("database_path={database:?}"))
		.with_option("database_migrations=false");

	with_started_server(&args, async |services| {
		services.db["eventid_resolvedstate"]
			.exists(NEW_MEMO)
			.await?;

		services.db["global"].exists(SENTINEL).await?;

		assert_authoritative_state(services).await?;

		services.db["global"].remove(MARKER);

		Ok(())
	})
}

fn reject_phase(database: &Path) -> Result {
	let args = Args::default_test(&[])
		.with_option(format!("database_path={database:?}"))
		.with_option("database_migrations=false");

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async_exec(&server));

	drop(runtime);

	assert!(
		matches!(result, Err(Error::Config(key, ..)) if key == "database_migrations"),
		"missing memo-migration marker did not refuse startup: {result:?}",
	);

	Ok(())
}

fn restore_phase(database: &Path) -> Result {
	let args = Args::default_test(&[]).with_option(format!("database_path={database:?}"));

	with_started_server(&args, async |services| {
		services.db["global"].exists(MARKER).await?;
		assert!(
			services.db["eventid_resolvedstate"]
				.exists(NEW_MEMO)
				.await
				.is_not_found()
		);

		services.db["global"].exists(SENTINEL).await?;

		assert_authoritative_state(services).await?;

		Ok(())
	})
}

async fn assert_authoritative_state(services: &Services) -> Result {
	let room_id = RoomId::parse(AUTHORITATIVE_ROOM)?;
	let shortstatehash: u64 = services.db["roomid_shortstatehash"]
		.get(room_id.as_bytes())
		.await
		.deserialized()?;

	assert_eq!(shortstatehash, AUTHORITATIVE_STATE_HASH);

	Ok(())
}

fn with_started_server<F>(args: &Args, inspect: F) -> Result
where
	F: AsyncFnOnce(&Services) -> Result,
{
	let runtime = Runtime::new(Some(args))?;
	let server = Server::new(Some(args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let outcome = inspect(&services).await;
		let shutdown = server.server.shutdown();

		drop(services);

		let run = async_run(&server).await;
		let stop = async_stop(&server).await;

		outcome.and(shutdown).and(run).and(stop)
	});

	drop(runtime);

	result
}
