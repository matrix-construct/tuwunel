#![cfg(test)]

use std::{env::temp_dir, fs::remove_dir_all};

use tuwunel::{Args, Runtime, Server, async_exec};
use tuwunel_core::Result;

/// MSC4025 admin surface: `user erasure` reports the marker state and `user
/// unerase` blind-deletes it, both against a freshly created (never-erased)
/// user.
#[test]
fn user_erasure_commands_roundtrip() -> Result {
	let db_dir = temp_dir().join("tuwunel-user-erasure-commands-test");

	let mut args = Args::default_test(&["smoke", "fresh", "cleanup"]);
	args.option
		.push(format!("database_path={:?}", db_dir.to_str().expect("utf-8 path")));
	args.execute
		.push("users create-user erasure_subject hunter2hunter2".into());
	args.execute
		.push("users erasure @erasure_subject:localhost".into());
	args.execute
		.push("users unerase @erasure_subject:localhost".into());
	args.execute
		.push("users erasure @erasure_subject:localhost".into());

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async { async_exec(&server).await });

	drop(runtime);
	remove_dir_all(&db_dir).ok();

	result
}
