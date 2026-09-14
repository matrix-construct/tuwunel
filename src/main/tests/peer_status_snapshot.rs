#![cfg(test)]

use std::fs::remove_dir_all;

use tuwunel::{Args, Runtime, Server, async_exec};
use tuwunel_core::Result;

/// Regression guard for the peer-status snapshot panic. `record-failure` writes
/// a `servername_status` row whose bucket key is `u64`; `snapshot` iterates the
/// column family and deserializes that key. A narrower key field has no
/// deserializer, so the first yielded row panics and the command fails.
#[test]
fn peer_status_snapshot_reads_recorded_failure() -> Result {
	let db_dir = Args::test_database_path("peer-status-snapshot");

	let mut args = Args::default_test(&["smoke", "fresh", "cleanup"]);
	args.option
		.push(format!("database_path={:?}", db_dir.to_str().expect("utf-8 path")));
	args.execute
		.push("query peer-status record-failure fail.example.com".into());
	args.execute
		.push("query peer-status snapshot".into());

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async { async_exec(&server).await });

	drop(runtime);
	remove_dir_all(&db_dir).ok();

	result
}
