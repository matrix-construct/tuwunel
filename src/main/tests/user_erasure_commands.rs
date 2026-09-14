#![cfg(test)]

use std::fs::remove_dir_all;

use tuwunel::{Args, Runtime, Server, async_exec};
use tuwunel_core::Result;

/// MSC4025 admin surface: `user erasure` reports the marker state and `user
/// unerase` blind-deletes it, both against a freshly created (never-erased)
/// user.
#[test]
fn user_erasure_commands_roundtrip() -> Result {
	let database = Args::test_database_path("user-erasure-commands");

	let args = Args::default_test(&["smoke", "fresh", "cleanup"])
		.with_database_path(&database)
		.with_execute("users create-user erasure_subject hunter2hunter2")
		.with_execute("users erasure @erasure_subject:localhost")
		.with_execute("users unerase @erasure_subject:localhost")
		.with_execute("users erasure @erasure_subject:localhost");

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async { async_exec(&server).await });

	drop(runtime);
	remove_dir_all(&database).ok();

	result
}
