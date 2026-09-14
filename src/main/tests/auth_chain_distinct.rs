#![cfg(test)]

use std::{fs::remove_dir_all, iter::once, path::PathBuf};

use futures::TryStreamExt;
use serde_json::json;
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result,
	result::NotFound,
	ruma::{
		CanonicalJsonObject, EventId, OwnedEventId, RoomId, RoomVersionId, event_id, room_id,
	},
};
use tuwunel_database::serialize_key;
use tuwunel_service::Services;

const NUM_BUCKETS: u64 = 50;

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

#[test]
fn auth_chain_is_distinct_and_caches_only_complete_walks() -> Result {
	let db_path = DatabasePath(Args::test_database_path("auth-chain-distinct"));

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
	let foreign_room_id = room_id!("!auth-chain-foreign:localhost");
	let left = event_id!("$left:localhost");
	let tail = event_id!("$tail:localhost");
	let torn = event_id!("$torn:localhost");
	let absent = event_id!("$absent:localhost");
	let stray = event_id!("$stray:localhost");
	let cross = event_id!("$cross:localhost");

	add_outlier(services, room_id, tail, &[])?;
	add_outlier(services, room_id, left, &[tail])?;
	add_outlier(services, room_id, torn, &[tail, absent])?;
	add_outlier(services, foreign_room_id, stray, &[])?;
	add_outlier(services, room_id, cross, &[stray])?;

	let left_short = services
		.short
		.get_or_create_shorteventid(left)
		.await;

	let torn_short = services
		.short
		.get_or_create_shorteventid(torn)
		.await;

	let cross_short = services
		.short
		.get_or_create_shorteventid(cross)
		.await;

	let right = mint_distinct_bucket(services, room_id, tail, left_short).await?;

	let room_version = RoomVersionId::V6;
	let cache = services.db.get("authchainkey_authchain")?;
	let left_key = serialize_key([left_short].as_slice())?;
	let torn_key = serialize_key([torn_short].as_slice())?;
	let cross_key = serialize_key([cross_short].as_slice())?;

	let foreign_chain = walk(services, foreign_room_id, &room_version, once(left)).await?;

	assert!(foreign_chain.is_empty(), "foreign room walk yields nothing");
	assert!(cache.exists(&left_key).await.is_not_found());

	let mut torn_chain = walk(services, room_id, &room_version, once(torn)).await?;

	torn_chain.sort_unstable();

	assert_eq!(torn_chain, [absent.to_owned(), tail.to_owned()]);
	assert!(cache.exists(&torn_key).await.is_not_found());

	let cross_chain = walk(services, room_id, &room_version, once(cross)).await?;

	assert_eq!(cross_chain, [stray.to_owned()]);
	assert!(cache.exists(&cross_key).await.is_not_found());

	let chain =
		walk(services, room_id, &room_version, [left, right.as_ref()].into_iter()).await?;

	assert_eq!(chain, [tail.to_owned()]);
	assert!(cache.exists(&left_key).await.is_ok(), "complete walk is memoized");

	let cached_chain =
		walk(services, room_id, &room_version, [left, right.as_ref()].into_iter()).await?;

	assert_eq!(cached_chain, [tail.to_owned()]);

	Ok(())
}

async fn walk<'a, I>(
	services: &'a Services,
	room_id: &'a RoomId,
	room_version: &'a RoomVersionId,
	starting_events: I,
) -> Result<Vec<OwnedEventId>>
where
	I: Iterator<Item = &'a EventId> + Clone + ExactSizeIterator + Send + 'a,
{
	services
		.auth_chain
		.event_ids_iter(room_id, room_version, starting_events)
		.try_collect()
		.await
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
