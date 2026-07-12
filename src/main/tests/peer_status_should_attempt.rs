#![cfg(test)]

use std::{env::temp_dir, fs::remove_dir_all, process::id as process_id};

use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{Result, err, ruma::OwnedServerName};
use tuwunel_service::federation::{Classification, ShouldAttempt};

/// Regression guard for the peer-status backoff scan. `should_attempt`
/// prefix-scans a server's failure rows to compute the verdict; a mis-seeded
/// seek yields an empty scan and returns `Yes`, silently disabling all backoff.
/// A peer that just failed must instead report `No`.
#[test]
fn should_attempt_backs_off_after_failure() -> Result {
	let db_dir = temp_dir().join(format!("tuwunel-peer-status-should-attempt-{}", process_id()));

	let mut args = Args::default_test(&["fresh", "cleanup"]);
	args.maintenance = true;
	args.option
		.push(format!("database_path={:?}", db_dir.to_str().expect("utf-8 path")));

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;

		let peer: OwnedServerName = "fail.example.com"
			.try_into()
			.expect("valid server name");

		services
			.federation
			.record_failure(&peer, Classification::Transient);

		let verdict = services.federation.should_attempt(&peer).await;

		server.server.shutdown()?;
		drop(services);

		async_run(&server).await?;
		async_stop(&server).await?;

		matches!(verdict, ShouldAttempt::No { .. })
			.then_some(())
			.ok_or_else(|| {
				err!("should_attempt returned {verdict:?} after a failure; backoff not applied")
			})
	});

	drop(runtime);
	remove_dir_all(&db_dir).ok();

	result
}
