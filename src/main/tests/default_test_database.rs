#![cfg(test)]

use std::{
	array,
	collections::HashSet,
	env::temp_dir,
	fs::{create_dir, read_to_string, remove_dir_all, write},
	path::PathBuf,
	thread,
};

use clap::Parser;
use tuwunel::{Args, Runtime, Server, args::update};
use tuwunel_core::{Result, config::Figment};
use tuwunel_database::Database;

/// Default tests must not inherit the database selected by a developer's
/// configuration, and reapplying the same arguments must retain their path.
#[test]
fn default_test_overrides_the_configured_database() -> Result {
	let args = Args::default_test(&["fresh", "cleanup"]);
	let configured = Figment::new().merge(("database_path", "configured-database"));
	let path: PathBuf = update(configured, &args)?.extract_inner("database_path")?;

	assert_ne!(path, PathBuf::from("configured-database"));
	assert_eq!(path.parent(), Some(temp_dir().as_path()));
	let cloned = args.clone();
	assert_eq!(database_path(&cloned)?, path);
	assert_eq!(database_path(&args)?, path);

	Ok(())
}

/// A shared path or a process-id-only path makes independent callers collide.
#[test]
fn concurrent_default_tests_have_distinct_databases() -> Result {
	let callers: [_; 8] = array::from_fn(|_| {
		thread::spawn(|| database_path(&Args::default_test(&["fresh", "cleanup"])))
	});
	let paths: HashSet<_> = callers
		.into_iter()
		.map(|caller| {
			caller
				.join()
				.expect("argument builder did not panic")
		})
		.collect::<Result<_>>()?;

	assert_eq!(paths.len(), 8);
	for path in paths {
		assert_eq!(path.parent(), Some(temp_dir().as_path()));
	}

	Ok(())
}

/// A test's explicit override still wins; ordinary CLI arguments gain no
/// test-specific database override.
#[test]
fn explicit_database_overrides_remain_available() -> Result {
	let explicit = temp_dir().join("chosen database with \"quotes\"");
	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option(format!("database_path={explicit:?}"));

	assert_eq!(database_path(&args)?, explicit);
	update(Figment::new(), &Args::parse_from(["tuwunel"]))?
		.find_value("database_path")
		.expect_err("ordinary CLI arguments must not choose a test database");

	Ok(())
}

/// Drive the real database's `fresh` and `cleanup` hooks with the default
/// path, while another default caller owns a neighboring directory.
#[test]
fn default_database_cleanup_leaves_other_tests_alone() -> Result {
	let mut args = Args::default_test(&["fresh", "cleanup"]);
	args.maintenance = true;
	let database = TestDirectory::create(database_path(&args)?)?;
	let neighbor =
		TestDirectory::create(database_path(&Args::default_test(&["fresh", "cleanup"]))?)?;
	let stale = database.0.join("before-fresh");
	let marker = neighbor.0.join("keep");
	write(&stale, "old test data")?;
	write(&marker, "another test's data")?;

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let db = runtime.block_on(Database::open(&server.server))?;

	assert_eq!(server.server.config.database_path, database.0);
	assert!(!stale.exists(), "fresh must remove this test's old database");
	assert_eq!(read_to_string(&marker)?, "another test's data");

	drop(db);
	drop(runtime);
	drop(server);

	assert!(!database.0.exists(), "cleanup must remove this test's database");
	assert_eq!(read_to_string(&marker)?, "another test's data");

	Ok(())
}

fn database_path(args: &Args) -> Result<PathBuf> {
	Ok(update(Figment::new(), args)?.extract_inner("database_path")?)
}

/// Own only directories this test created successfully, including on failure.
struct TestDirectory(PathBuf);

impl TestDirectory {
	#[expect(
		clippy::create_dir,
		reason = "only claim and clean up a directory when this test creates it"
	)]
	fn create(path: PathBuf) -> Result<Self> {
		assert_eq!(path.parent(), Some(temp_dir().as_path()));
		create_dir(&path)?;
		Ok(Self(path))
	}
}

impl Drop for TestDirectory {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}
