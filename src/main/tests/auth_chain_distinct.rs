#![cfg(test)]

use std::{env::var, fs::remove_dir_all, path::PathBuf, process::id as process_id};

use futures::TryStreamExt;
use serde_json::json;
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result,
	ruma::{
		CanonicalJsonObject, EventId, OwnedEventId, RoomId, RoomVersionId, event_id, room_id,
	},
};
use tuwunel_service::Services;

const NUM_BUCKETS: u64 = 50;

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

#[test]
fn event_ids_iter_is_distinct_for_auth_diamond() -> Result {
	let root = var("TMPDIR").unwrap_or_else(|_| "/nvme/target/tmp".into());
	let path = PathBuf::from(root).join(format!("tuwunel-auth-chain-distinct-{}", process_id()));
	let db_path = DatabasePath(path);

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
	let room_id = room_id!("!auth-chain-distinct:localhost");
	let left = event_id!("$left:localhost");
	let tail = event_id!("$tail:localhost");

	add_outlier(services, room_id, tail, &[])?;
	add_outlier(services, room_id, left, &[tail])?;

	let left_short = services
		.short
		.get_or_create_shorteventid(left)
		.await;

	let right = mint_distinct_bucket(services, room_id, tail, left_short).await?;

	let room_version = RoomVersionId::V6;
	let chain: Vec<_> = services
		.auth_chain
		.event_ids_iter(room_id, &room_version, [left, right.as_ref()].into_iter())
		.try_collect()
		.await?;

	assert_eq!(chain, [tail.to_owned()]);

	Ok(())
}

/// Mints right-side events until one lands outside the left bucket.
///
/// Separate buckets allow the convergent auth-chain walks to overlap;
/// concurrent allocations could collide them.
async fn mint_distinct_bucket(
	services: &Services,
	room_id: &RoomId,
	tail: &EventId,
	left_short: u64,
) -> Result<OwnedEventId> {
	for attempt in 0..NUM_BUCKETS {
		let right = OwnedEventId::try_from(format!("$right-{attempt}:localhost"))?;

		add_outlier(services, room_id, &right, &[tail])?;

		let right_short = services
			.short
			.get_or_create_shorteventid(&right)
			.await;

		if right_short % NUM_BUCKETS != left_short % NUM_BUCKETS {
			return Ok(right);
		}
	}

	panic!("bucket separation must converge");
}

fn add_outlier(
	services: &Services,
	room_id: &RoomId,
	event_id: &EventId,
	auth_events: &[&EventId],
) -> Result {
	let pdu = json!({
		"auth_events": auth_events,
		"room_id": room_id,
	});

	let pdu: CanonicalJsonObject = serde_json::from_value(pdu)?;

	services.timeline.add_pdu_outlier(event_id, &pdu);

	Ok(())
}
