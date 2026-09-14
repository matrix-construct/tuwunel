#![cfg(test)]

use std::{fs::remove_dir_all, path::PathBuf};

use futures::{StreamExt, pin_mut};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result,
	ruma::{OwnedEventId, event_id},
	utils::stream::ReadyExt,
};
use tuwunel_service::Services;

const OCCURRENCES: usize = 8;

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

#[test]
fn batch_duplicates_share_one_shorteventid() -> Result {
	let db_path = DatabasePath(Args::test_database_path("short-id-allocation"));

	let mut args = Args::default_test(&["fresh", "cleanup"]);
	args.maintenance = true;
	args.option
		.push(format!("database_path={:?}", db_path.0));

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let outcome = exercise(&services).await;
		let shutdown = server.server.shutdown();

		drop(services);

		let run = async_run(&server).await;
		let stop = async_stop(&server).await;

		outcome.and(shutdown).and(run).and(stop)
	});

	drop(runtime);

	result
}

async fn exercise(services: &Services) -> Result {
	let event_id = event_id!("$short-id-allocation-batch:localhost");
	// a repeated event misses the batched lookup on every occurrence
	let batch = [event_id; OCCURRENCES];

	let shorts = services
		.short
		.multi_get_or_create_shorteventid(batch.iter().copied());

	pin_mut!(shorts);

	let Some(first) = shorts.next().await else {
		return Err!("batch yielded no short ids");
	};

	if shorts.ready_any(|short| short.ne(&first)).await {
		return Err!("one event id took more than one short id within a batch");
	}

	let resolved: OwnedEventId = services
		.short
		.get_eventid_from_short(first)
		.await?;

	if resolved != event_id {
		return Err!("short id did not resolve back to its event id");
	}

	Ok(())
}
