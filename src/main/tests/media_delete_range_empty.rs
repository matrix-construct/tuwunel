#![cfg(test)]

use std::{fs::remove_dir_all, process::id as process_id};

use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_admin::{fini, init};
use tuwunel_core::{
	Err, Result,
	utils::time::{now, timepoint_from_epoch},
};
use tuwunel_service::Services;

/// An empty eligible set is zero deletions, not an error: `delete_range` with
/// the purge_media_cache argument shape returns Ok(0), and the `media
/// delete-range` admin command reports zero deleted files.
#[test]
fn media_delete_range_empty_set() -> Result {
	// Isolate the database under /tmp so parallel test binaries do not contend.
	let db_path = format!("/tmp/tuwunel-test-media-delete-range-empty-{}", process_id());

	let mut args = Args::default_test(&["fresh", "cleanup"]);
	args.maintenance = true;
	args.option
		.push(format!("database_path=\"{db_path}\""));

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;

	let result: Result = runtime.block_on(async {
		let services = async_start(&server).await?;

		init(&services.admin);

		let outcome = empty_delete_range_is_zero(&services).await;

		fini(&services.admin);
		server.server.shutdown()?;
		drop(services);

		async_run(&server).await?;
		async_stop(&server).await?;

		outcome
	});

	drop(runtime);

	remove_dir_all(&db_path).ok();

	result
}

async fn empty_delete_range_is_zero(services: &Services) -> Result {
	let cutoff = timepoint_from_epoch(now())?;

	let deleted = services
		.media
		.delete_range(cutoff, true, false, false)
		.await?;

	if deleted != 0 {
		return Err!("expected zero deletions over an empty media set: {deleted}");
	}

	match services
		.admin
		.command_in_place("media delete-range 7d --older-than".into(), None)
		.await
	{
		| Ok(Some(output)) if output.as_str().contains("Deleted 0 total files.") => Ok(()),
		| Ok(None) => Err!("delete-range command produced no output"),
		| Ok(Some(output)) | Err(output) => {
			let output = output.as_str();

			Err!("unexpected delete-range output: {output}")
		},
	}
}
