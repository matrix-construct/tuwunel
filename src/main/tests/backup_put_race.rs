#![cfg(test)]

use std::{
	collections::BTreeMap, fs::remove_dir_all, iter::once, net::TcpListener, pin::Pin,
	task::Poll, time::Duration,
};

use futures::{
	Stream, StreamExt,
	channel::oneshot::{Receiver, Sender, channel},
	future::{Either, join, join4, poll_fn, ready, select},
	stream::{iter, once as once_stream},
};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::time::timeout;
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Error, Result,
	ruma::{
		OwnedRoomId, OwnedUserId, RoomId, UserId,
		api::client::backup::{BackupAlgorithm, KeyBackupData},
		serde::Raw,
	},
	utils::stream::IterStream,
};
use tuwunel_database::Json;
use tuwunel_service::Services;

use self::client::{Client, register, wait_until_ready};

mod client;

const ACCESS_TOKEN: &str = "backup-put-race-access-token-00000000";
const DELETE_TOKEN: &str = "backup-put-race-delete-token-00000000";
const PARTIAL_TOKEN: &str = "backup-put-race-partial-token-00000000";
const ALIAS_TOKEN: &str = "backup-put-race-alias-token-00000000";

type BackupKey<'a> = (&'a RoomId, &'a str, &'a Raw<KeyBackupData>);
type OwnedFuture<F> = Pin<Box<F>>;

struct QualityOrder<'a> {
	user_id: &'a UserId,
	room: &'a RoomId,
	label: &'a str,
	best: &'a Raw<KeyBackupData>,
	worse: &'a Raw<KeyBackupData>,
	best_first: bool,
}

#[test]
fn backup_put_mutations_are_serialized() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let db_path = Args::test_database_path("backup-put-race");
	let args = Args::default_test(&["fresh", "cleanup"])
		.with_database_path(&db_path)
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={port}"))
		.with_option("listening=true");

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let exercise_task = async {
			let outcome = exercise(&services, &base).await;
			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (run_result, outcome) = join(async_run(&server), exercise_task).await;

		drop(services);
		async_stop(&server).await?;
		run_result?;

		outcome
	});

	drop(runtime);
	remove_dir_all(&db_path).ok();

	result
}

async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;

	put_first_create_waits(services).await?;
	create_first_and_empty_puts(services, base).await?;
	put_first_delete_waits(services).await?;
	delete_first(services, base).await?;
	other_writers_wait(services).await?;
	update_delete_orders(services).await?;
	key_delete_version_delete_orders(services).await?;
	competing_key_quality(services).await?;
	mutation_metadata_is_captured(services).await?;
	partial_failure_is_retained(services, base).await?;
	active_cancellation_releases_waiter(services).await?;
	queued_cancellation_releases_waiter(services).await?;
	exact_count_prefix(services).await?;
	numeric_latest_versions(services).await?;
	exact_existence_after_latest(services, base).await?;

	Ok(())
}

async fn put_first_create_waits(services: &Services) -> Result {
	let user_id = direct_user(services, "put-first-create")?;
	let other_id = direct_user(services, "put-first-other")?;
	let other_room = room("put-first-other")?;
	let room = room("put-first-create")?;
	let old_algorithm = algorithm("put-first-old");
	let next_algorithm = algorithm("put-first-next");
	let first = key_data("put-first-a", true, 0, 0);
	let second = key_data("put-first-b", true, 0, 0);
	let version = services
		.key_backups
		.create_backup(&user_id, &old_algorithm)
		.await?;

	let (keys, reached, release) =
		paused_keys((&room, "first", &first), Some((&room, "second", &second)));

	let upload = services
		.key_backups
		.add_keys(&user_id, &version, keys);

	let upload = watch(wait_paused(upload, reached), "PUT did not reach its pause").await;

	assert_ciphertext(services, &user_id, &version, &room, "first", "put-first-a").await?;

	let create = services
		.key_backups
		.create_backup(&user_id, &next_algorithm);

	let create = pending_once(create, "same-user create ran during PUT").await;

	assert_eq!(
		services
			.key_backups
			.get_latest_backup_version(&user_id)
			.await?,
		version,
		"waiting create advanced the latest version",
	);

	let other_user_mutation = async {
		let other_algorithm = algorithm("put-first-other");
		let other_key = key_data("put-first-other", true, 0, 0);
		let other_version = services
			.key_backups
			.create_backup(&other_id, &other_algorithm)
			.await?;

		add_key(services, &other_id, &other_version, &other_room, "other", &other_key).await?;

		services
			.key_backups
			.delete_backup(&other_id, &other_version)
			.await;

		assert_missing_backup(services, &other_id, &other_version).await;

		Ok::<(), Error>(())
	};

	watch(other_user_mutation, "another user's mutation was globally blocked").await?;

	assert_eq!(
		services
			.key_backups
			.get_latest_backup_version(&user_id)
			.await?,
		version,
		"another user changed the paused user's version",
	);

	release
		.send(())
		.expect("PUT release receiver disappeared");

	let (upload_result, create_result) =
		watch(join(upload, create), "PUT and waiting create did not finish").await;

	let (count, _) = upload_result?;
	let next = create_result?;

	assert_eq!(count, 2, "PUT returned the wrong key count");
	assert_eq!(
		services
			.key_backups
			.count_keys(&user_id, &next)
			.await,
		0,
		"subsequent version inherited keys",
	);

	services
		.key_backups
		.delete_backup(&user_id, &version)
		.await;

	services
		.key_backups
		.delete_backup(&user_id, &next)
		.await;

	Ok(())
}

async fn create_first_and_empty_puts(services: &Services, base: &str) -> Result {
	let user_id = register(services, "backup_put_create_first", ACCESS_TOKEN).await?;
	let client = Client { services, base, token: ACCESS_TOKEN };
	let room = client.create_room(&json!({})).await?;
	let old_algorithm = algorithm("create-first-old");
	let latest_algorithm = algorithm("create-first-latest");
	let old_key = key_data("create-first-old", true, 0, 0);
	let stale_key = key_data("create-first-stale", true, 0, 0);
	let old = services
		.key_backups
		.create_backup(&user_id, &old_algorithm)
		.await?;

	add_key(services, &user_id, &old, &room, "old", &old_key).await?;

	let old_state = services
		.key_backups
		.get_count_etag(&user_id, &old)
		.await?;

	let latest = services
		.key_backups
		.create_backup(&user_id, &latest_algorithm)
		.await?;

	let stale_routes = [
		("room_keys/keys".to_owned(), bulk_body(&room, &[("bulk", &stale_key)])),
		(format!("room_keys/keys/{room}"), room_body(&[("room", &stale_key)])),
		(format!("room_keys/keys/{room}/session"), raw_value(&stale_key)),
	];

	for (path, body) in stale_routes {
		let (status, response) = put_json(&client, &path, &old, &body).await?;

		assert_wrong_version(status, &response, &latest);
	}

	for (path, body) in [
		("room_keys/keys".to_owned(), bulk_body(&room, &[])),
		(format!("room_keys/keys/{room}"), room_body(&[])),
	] {
		let (status, response) = put_json(&client, &path, &old, &body).await?;

		assert_wrong_version(status, &response, &latest);
	}

	let latest_state = services
		.key_backups
		.get_count_etag(&user_id, &latest)
		.await?;

	for (path, body) in [
		("room_keys/keys".to_owned(), bulk_body(&room, &[])),
		(format!("room_keys/keys/{room}"), room_body(&[])),
	] {
		let (status, response) = put_json(&client, &path, &latest, &body).await?;

		assert_eq!(status, StatusCode::OK, "{response}");
		assert_eq!(response["count"], json!(latest_state.0), "{response}");
		assert_eq!(response["etag"], latest_state.1.to_string(), "{response}");
	}

	let observed_old_state = services
		.key_backups
		.get_count_etag(&user_id, &old)
		.await?;

	assert_eq!(observed_old_state, old_state, "stale PUT changed old metadata");

	assert_ciphertext(services, &user_id, &old, &room, "old", "create-first-old").await?;

	services
		.key_backups
		.delete_backup(&user_id, &old)
		.await;

	services
		.key_backups
		.delete_backup(&user_id, &latest)
		.await;

	Ok(())
}

async fn put_first_delete_waits(services: &Services) -> Result {
	let user_id = direct_user(services, "put-first-delete")?;
	let room = room("put-first-delete")?;
	let algorithm = algorithm("put-first-delete");
	let first = key_data("put-delete-a", true, 0, 0);
	let second = key_data("put-delete-b", true, 0, 0);
	let version = services
		.key_backups
		.create_backup(&user_id, &algorithm)
		.await?;

	let (keys, reached, release) =
		paused_keys((&room, "first", &first), Some((&room, "second", &second)));

	let upload = services
		.key_backups
		.add_keys(&user_id, &version, keys);

	let upload = watch(wait_paused(upload, reached), "PUT did not reach delete pause").await;
	let delete = services
		.key_backups
		.delete_backup(&user_id, &version);

	let delete = pending_once(delete, "version delete ran during PUT").await;

	services
		.key_backups
		.get_backup(&user_id, &version)
		.await?;

	services
		.key_backups
		.get_etag(&user_id, &version)
		.await?;

	assert_ciphertext(services, &user_id, &version, &room, "first", "put-delete-a").await?;

	release
		.send(())
		.expect("delete release receiver disappeared");

	let (upload_result, ()) =
		watch(join(upload, delete), "PUT and version delete did not finish").await;

	upload_result?;
	assert_missing_backup(services, &user_id, &version).await;
	assert_missing_key(services, &user_id, &version, &room, "first").await;
	assert_missing_key(services, &user_id, &version, &room, "second").await;

	services
		.key_backups
		.delete_backup(&user_id, &version)
		.await;

	assert_missing_backup(services, &user_id, &version).await;

	Ok(())
}

async fn delete_first(services: &Services, base: &str) -> Result {
	let user_id = register(services, "backup_put_delete_first", DELETE_TOKEN).await?;
	let client = Client { services, base, token: DELETE_TOKEN };
	let room = client.create_room(&json!({})).await?;
	let algorithm = algorithm("delete-first");
	let key = key_data("delete-first", true, 0, 0);
	let old = services
		.key_backups
		.create_backup(&user_id, &algorithm)
		.await?;

	let latest = services
		.key_backups
		.create_backup(&user_id, &algorithm)
		.await?;

	services
		.key_backups
		.delete_backup(&user_id, &old)
		.await;

	let path = format!("room_keys/keys/{room}/old");
	let (status, body) = put_json(&client, &path, &old, &raw_value(&key)).await?;

	assert_wrong_version(status, &body, &latest);
	assert_missing_key(services, &user_id, &old, &room, "old").await;

	services
		.key_backups
		.delete_backup(&user_id, &latest)
		.await;

	let path = format!("room_keys/keys/{room}/latest");
	let (status, body) = put_json(&client, &path, &latest, &raw_value(&key)).await?;

	assert_not_found(status, &body);
	assert_missing_key(services, &user_id, &latest, &room, "latest").await;

	Ok(())
}

async fn other_writers_wait(services: &Services) -> Result {
	let user_id = direct_user(services, "other-writers")?;
	let room = room("other-writers")?;
	let old_algorithm = algorithm("other-writers-old");
	let updated_algorithm = algorithm("other-writers-updated");
	let first = key_data("other-writers-a", true, 0, 0);
	let second = key_data("other-writers-b", true, 0, 0);
	let version = services
		.key_backups
		.create_backup(&user_id, &old_algorithm)
		.await?;

	let (keys, reached, release) =
		paused_keys((&room, "first", &first), Some((&room, "second", &second)));

	let upload = services
		.key_backups
		.add_keys(&user_id, &version, keys);

	let upload = watch(wait_paused(upload, reached), "PUT did not reach writer pause").await;
	let update = services
		.key_backups
		.update_backup(&user_id, &version, &updated_algorithm);

	let update = pending_once(update, "metadata update ran during PUT").await;
	let session_delete = services
		.key_backups
		.delete_room_key(&user_id, &version, &room, "first");

	let session_delete = pending_once(session_delete, "session delete ran during PUT").await;
	let room_delete = services
		.key_backups
		.delete_room_keys(&user_id, &version, &room);

	let room_delete = pending_once(room_delete, "room delete ran during PUT").await;
	let all_delete = services
		.key_backups
		.delete_all_keys(&user_id, &version);

	let all_delete = pending_once(all_delete, "all-keys delete ran during PUT").await;

	services
		.key_backups
		.get_backup(&user_id, &version)
		.await?;

	assert_ciphertext(services, &user_id, &version, &room, "first", "other-writers-a").await?;

	release
		.send(())
		.expect("writer release receiver disappeared");

	let (upload_result, (update_result, session_result, room_result, all_result)) = watch(
		join(upload, join4(update, session_delete, room_delete, all_delete)),
		"queued writers did not finish",
	)
	.await;

	upload_result?;
	update_result?;
	session_result?;
	room_result?;
	let (count, _) = all_result?;

	assert_eq!(count, 0, "all-keys delete left keys behind");
	assert_algorithm(services, &user_id, &version, "other-writers-updated").await?;

	services
		.key_backups
		.delete_backup(&user_id, &version)
		.await;

	Ok(())
}

async fn update_delete_orders(services: &Services) -> Result {
	let user_id = direct_user(services, "update-delete-orders")?;
	let room = room("update-delete-orders")?;
	let old_algorithm = algorithm("update-delete-old");
	let updated = algorithm("update-delete-new");
	let key = key_data("update-delete", true, 0, 0);
	let deleted_first =
		seeded_backup(services, &user_id, &room, "deleted-first", &old_algorithm, &key).await?;

	services
		.key_backups
		.delete_backup(&user_id, &deleted_first)
		.await;

	let deleted_update = services
		.key_backups
		.update_backup(&user_id, &deleted_first, &updated)
		.await;

	assert!(deleted_update.is_err(), "update recreated deleted metadata");

	assert_missing_backup(services, &user_id, &deleted_first).await;
	assert_missing_key(services, &user_id, &deleted_first, &room, "deleted-first").await;

	let updated_first =
		seeded_backup(services, &user_id, &room, "updated-first", &old_algorithm, &key).await?;

	services
		.key_backups
		.update_backup(&user_id, &updated_first, &updated)
		.await?;

	services
		.key_backups
		.delete_backup(&user_id, &updated_first)
		.await;

	assert_missing_backup(services, &user_id, &updated_first).await;
	assert_missing_key(services, &user_id, &updated_first, &room, "updated-first").await;

	Ok(())
}

async fn key_delete_version_delete_orders(services: &Services) -> Result {
	let user_id = direct_user(services, "key-version-delete-orders")?;
	let room = room("key-version-delete-orders")?;
	let algorithm = algorithm("key-version-delete");
	let key = key_data("key-version-delete", true, 0, 0);
	let key_first =
		seeded_backup(services, &user_id, &room, "key-first", &algorithm, &key).await?;

	services
		.key_backups
		.delete_room_key(&user_id, &key_first, &room, "key-first")
		.await?;

	services
		.key_backups
		.delete_backup(&user_id, &key_first)
		.await;

	assert_missing_backup(services, &user_id, &key_first).await;

	let version_first =
		seeded_backup(services, &user_id, &room, "version-first", &algorithm, &key).await?;

	services
		.key_backups
		.delete_backup(&user_id, &version_first)
		.await;

	let late_delete = services
		.key_backups
		.delete_room_key(&user_id, &version_first, &room, "version-first")
		.await;

	assert!(late_delete.is_err(), "key delete accepted a deleted version");

	assert_missing_backup(services, &user_id, &version_first).await;
	assert_missing_key(services, &user_id, &version_first, &room, "version-first").await;

	Ok(())
}

async fn competing_key_quality(services: &Services) -> Result {
	let user_id = direct_user(services, "competing-key-quality")?;
	let room = room("competing-key-quality")?;

	for (label, best, worse) in [
		(
			"verified",
			key_data("verified-best", true, 9, 9),
			key_data("verified-worse", false, 0, 0),
		),
		(
			"first-index",
			key_data("first-index-best", true, 1, 9),
			key_data("first-index-worse", true, 9, 0),
		),
		(
			"forwarded-count",
			key_data("forwarded-count-best", true, 1, 1),
			key_data("forwarded-count-worse", true, 1, 9),
		),
	] {
		for best_first in [true, false] {
			let order = QualityOrder {
				user_id: &user_id,
				room: &room,
				label,
				best: &best,
				worse: &worse,
				best_first,
			};

			quality_order(services, order).await?;
		}
	}

	let algorithm = algorithm("equal-worse");
	let best = key_data("equal-best", true, 1, 1);
	let equal = key_data("equal-candidate", true, 1, 1);
	let worse = key_data("worse-candidate", true, 2, 0);
	let version = services
		.key_backups
		.create_backup(&user_id, &algorithm)
		.await?;

	add_key(services, &user_id, &version, &room, "equal", &best).await?;

	let before = services
		.key_backups
		.get_count_etag(&user_id, &version)
		.await?;

	let equal_result = add_key(services, &user_id, &version, &room, "equal", &equal).await?;
	let worse_result = add_key(services, &user_id, &version, &room, "equal", &worse).await?;

	assert_eq!(equal_result, before, "equal key changed metadata");
	assert_eq!(worse_result, before, "worse key changed metadata");
	assert_ciphertext(services, &user_id, &version, &room, "equal", "equal-best").await?;

	services
		.key_backups
		.delete_backup(&user_id, &version)
		.await;

	Ok(())
}

async fn quality_order(services: &Services, order: QualityOrder<'_>) -> Result {
	let QualityOrder {
		user_id,
		room,
		label,
		best,
		worse,
		best_first,
	} = order;

	let name = if best_first { "best-first" } else { "worse-first" };
	let algorithm = algorithm(&format!("{label}-{name}"));
	let version = services
		.key_backups
		.create_backup(user_id, &algorithm)
		.await?;

	let (first, second) = if best_first { (best, worse) } else { (worse, best) };

	add_key(services, user_id, &version, room, label, first).await?;
	add_key(services, user_id, &version, room, label, second).await?;

	assert_eq!(
		services
			.key_backups
			.count_keys(user_id, &version)
			.await,
		1,
		"quality race produced duplicate keys",
	);

	assert_ciphertext(services, user_id, &version, room, label, &format!("{label}-best")).await?;

	services
		.key_backups
		.delete_backup(user_id, &version)
		.await;

	Ok(())
}

async fn mutation_metadata_is_captured(services: &Services) -> Result {
	let user_id = direct_user(services, "mutation-metadata")?;
	let room = room("mutation-metadata")?;
	let algorithm = algorithm("mutation-metadata");
	let first = key_data("mutation-first", true, 0, 0);
	let follower = key_data("mutation-follower", true, 0, 0);
	let version = services
		.key_backups
		.create_backup(&user_id, &algorithm)
		.await?;

	let (keys, reached, release) = paused_keys((&room, "first", &first), None);
	let upload = services
		.key_backups
		.add_keys(&user_id, &version, keys);

	let upload = watch(wait_paused(upload, reached), "metadata PUT did not pause").await;
	let expected = services
		.key_backups
		.get_count_etag(&user_id, &version)
		.await?;

	let room_id: &RoomId = &room;
	let keys = once((room_id, "follower", &follower)).stream();
	let follower = services
		.key_backups
		.add_keys(&user_id, &version, keys);

	let follower = pending_once(follower, "following writer ran during metadata PUT").await;

	assert_eq!(expected.0, 1, "paused upload did not commit its real key");

	release
		.send(())
		.expect("metadata release receiver disappeared");

	let (upload_result, follower_result) =
		watch(join(upload, follower), "metadata writers did not finish").await;

	let upload_metadata = upload_result?;
	let follower_metadata = follower_result?;

	assert_eq!(upload_metadata, expected, "PUT returned a later writer's metadata");
	assert_eq!(follower_metadata.0, 2, "following writer returned the wrong count");
	assert!(
		follower_metadata.1 > upload_metadata.1,
		"following writer did not advance the etag",
	);

	services
		.key_backups
		.delete_backup(&user_id, &version)
		.await;

	Ok(())
}

async fn partial_failure_is_retained(services: &Services, base: &str) -> Result {
	let user_id = register(services, "backup_put_partial", PARTIAL_TOKEN).await?;
	let client = Client { services, base, token: PARTIAL_TOKEN };
	let room = client.create_room(&json!({})).await?;
	let algorithm = algorithm("partial-failure");
	let old = key_data("partial-old", true, 0, 0);
	let valid = key_data("partial-valid", true, 0, 0);
	let malformed = malformed_key("partial-malformed");
	let version = services
		.key_backups
		.create_backup(&user_id, &algorithm)
		.await?;

	add_key(services, &user_id, &version, &room, "z-bad", &old).await?;

	let before = services
		.key_backups
		.get_count_etag(&user_id, &version)
		.await?;

	let body = bulk_body(&room, &[("a-valid", &valid), ("z-bad", &malformed)]);
	let (status, response) = put_json(&client, "room_keys/keys", &version, &body).await?;

	assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
	assert_eq!(response["errcode"], "M_BAD_JSON", "{response}");
	assert_ciphertext(services, &user_id, &version, &room, "a-valid", "partial-valid").await?;
	assert_ciphertext(services, &user_id, &version, &room, "z-bad", "partial-old").await?;

	let after = services
		.key_backups
		.get_count_etag(&user_id, &version)
		.await?;

	assert_eq!(after.0, 2, "partial failure retained the wrong key count");
	assert!(after.1 > before.1, "partial failure lost the committed etag transition");

	services
		.key_backups
		.delete_backup(&user_id, &version)
		.await;

	Ok(())
}

async fn active_cancellation_releases_waiter(services: &Services) -> Result {
	let user_id = direct_user(services, "active-cancellation")?;
	let room = room("active-cancellation")?;
	let old_algorithm = algorithm("active-cancellation");
	let next_algorithm = algorithm("active-cancellation-next");
	let first = key_data("active-first", true, 0, 0);
	let second = key_data("active-second", true, 0, 0);
	let version = services
		.key_backups
		.create_backup(&user_id, &old_algorithm)
		.await?;

	let (keys, reached, release) =
		paused_keys((&room, "first", &first), Some((&room, "second", &second)));

	let upload = services
		.key_backups
		.add_keys(&user_id, &version, keys);

	let upload = watch(wait_paused(upload, reached), "cancelled PUT did not pause").await;
	let create = services
		.key_backups
		.create_backup(&user_id, &next_algorithm);

	let create = pending_once(create, "waiter ran before active cancellation").await;

	drop(upload);
	drop(release);

	let next = watch(create, "waiter did not finish after active cancellation").await?;

	assert_ciphertext(services, &user_id, &version, &room, "first", "active-first").await?;
	assert_missing_key(services, &user_id, &version, &room, "second").await;
	let count = services
		.key_backups
		.count_keys(&user_id, &next)
		.await;

	assert_eq!(count, 0, "waiter created a nonempty version");

	services
		.key_backups
		.delete_backup(&user_id, &version)
		.await;

	services
		.key_backups
		.delete_backup(&user_id, &next)
		.await;

	Ok(())
}

async fn queued_cancellation_releases_waiter(services: &Services) -> Result {
	let user_id = direct_user(services, "queued-cancellation")?;
	let room = room("queued-cancellation")?;
	let old_algorithm = algorithm("queued-cancellation-old");
	let canceled_algorithm = algorithm("queued-cancellation-canceled");
	let next_algorithm = algorithm("queued-cancellation-next");
	let first = key_data("queued-first", true, 0, 0);
	let second = key_data("queued-second", true, 0, 0);
	let version = services
		.key_backups
		.create_backup(&user_id, &old_algorithm)
		.await?;

	let (keys, reached, release) =
		paused_keys((&room, "first", &first), Some((&room, "second", &second)));

	let upload = services
		.key_backups
		.add_keys(&user_id, &version, keys);

	let upload =
		watch(wait_paused(upload, reached), "PUT did not reach queued cancellation").await;

	let canceled = services
		.key_backups
		.update_backup(&user_id, &version, &canceled_algorithm);

	let canceled = pending_once(canceled, "queued update ran during PUT").await;

	drop(canceled);

	let create = services
		.key_backups
		.create_backup(&user_id, &next_algorithm);

	let create = pending_once(create, "surviving waiter ran during PUT").await;

	release
		.send(())
		.expect("cancellation release receiver disappeared");

	let (upload_result, create_result) =
		watch(join(upload, create), "surviving waiter did not finish").await;

	let next = create_result?;

	upload_result?;
	assert_algorithm(services, &user_id, &version, "queued-cancellation-old").await?;

	let count = services
		.key_backups
		.count_keys(&user_id, &version)
		.await;

	assert_eq!(count, 2, "queued cancellation changed the upload");

	services
		.key_backups
		.delete_backup(&user_id, &version)
		.await;

	services
		.key_backups
		.delete_backup(&user_id, &next)
		.await;

	Ok(())
}

async fn exact_count_prefix(services: &Services) -> Result {
	let user_id = direct_user(services, "exact-count-prefix")?;
	let other_id = direct_user(services, "exact-count-prefix-other")?;
	let room = room("exact-count-prefix")?;
	let algorithm = algorithm("exact-count-prefix");
	let key = key_data("exact-count-prefix", true, 0, 0);

	seed_metadata(services, &user_id, "1", &algorithm, 101);
	seed_metadata(services, &user_id, "10", &algorithm, 110);
	seed_metadata(services, &other_id, "1", &algorithm, 201);
	for session in ["one-a", "one-b"] {
		seed_key(services, &user_id, "1", &room, session, &key);
	}

	for session in ["ten-a", "ten-b", "ten-c"] {
		seed_key(services, &user_id, "10", &room, session, &key);
	}

	for session in ["other-a", "other-b", "other-c", "other-d"] {
		seed_key(services, &other_id, "1", &room, session, &key);
	}

	let one_count = services
		.key_backups
		.count_keys(&user_id, "1")
		.await;

	let ten_count = services
		.key_backups
		.count_keys(&user_id, "10")
		.await;

	let other_count = services
		.key_backups
		.count_keys(&other_id, "1")
		.await;

	let latest = services
		.key_backups
		.get_latest_backup_version(&user_id)
		.await?;

	assert_eq!(one_count, 2);
	assert_eq!(ten_count, 3);
	assert_eq!(other_count, 4);
	assert_eq!(latest, "10");

	services
		.key_backups
		.delete_backup(&user_id, "1")
		.await;

	services
		.key_backups
		.delete_backup(&user_id, "10")
		.await;

	services
		.key_backups
		.delete_backup(&other_id, "1")
		.await;

	Ok(())
}

async fn numeric_latest_versions(services: &Services) -> Result {
	let algorithm = algorithm("numeric-latest");
	let numeric_id = direct_user(services, "numeric-latest")?;
	let zero_id = direct_user(services, "numeric-latest-zero")?;
	let absent_id = direct_user(services, "numeric-latest-absent")?;
	let nonnumeric_id = direct_user(services, "numeric-latest-nonnumeric")?;

	seed_metadata(services, &numeric_id, "9", &algorithm, 9);
	seed_metadata(services, &numeric_id, "10", &algorithm, 10);

	let latest = services
		.key_backups
		.get_latest_backup_version(&numeric_id)
		.await?;

	assert_eq!(latest, "10");

	seed_metadata(services, &zero_id, "0", &algorithm, 1);

	let latest = services
		.key_backups
		.get_latest_backup_version(&zero_id)
		.await?;

	let absent = services
		.key_backups
		.get_latest_backup_version(&absent_id)
		.await;

	assert_eq!(latest, "0");
	assert!(absent.is_err(), "absent user returned a latest version");

	seed_metadata(services, &nonnumeric_id, "legacy", &algorithm, 1);

	let nonnumeric = services
		.key_backups
		.get_latest_backup_version(&nonnumeric_id)
		.await;

	assert!(nonnumeric.is_err(), "nonnumeric version became latest");

	for (user_id, version) in [
		(numeric_id.as_ref(), "9"),
		(numeric_id.as_ref(), "10"),
		(zero_id.as_ref(), "0"),
		(nonnumeric_id.as_ref(), "legacy"),
	] {
		services
			.key_backups
			.delete_backup(user_id, version)
			.await;
	}

	Ok(())
}

async fn exact_existence_after_latest(services: &Services, base: &str) -> Result {
	let user_id = register(services, "backup_put_alias", ALIAS_TOKEN).await?;
	let client = Client { services, base, token: ALIAS_TOKEN };
	let room = client.create_room(&json!({})).await?;
	let algorithm = algorithm("exact-existence-alias");
	let key = key_data("exact-existence-alias", true, 0, 0);

	seed_metadata(services, &user_id, "01", &algorithm, 1);

	let latest = services
		.key_backups
		.get_latest_backup_version(&user_id)
		.await?;

	assert_eq!(latest, "1");

	let path = format!("room_keys/keys/{room}/canonical");
	let (status, body) = put_json(&client, &path, "1", &raw_value(&key)).await?;

	assert_not_found(status, &body);

	let canonical_algorithm = services
		.key_backups
		.get_backup(&user_id, "1")
		.await;

	let canonical_etag = services.key_backups.get_etag(&user_id, "1").await;

	assert!(canonical_algorithm.is_err(), "canonical PUT created algorithm metadata");
	assert!(canonical_etag.is_err(), "canonical PUT created an etag");

	assert_missing_key(services, &user_id, "1", &room, "canonical").await;

	let alias_path = format!("room_keys/keys/{room}/alias");
	let (status, body) = put_json(&client, &alias_path, "01", &raw_value(&key)).await?;

	assert_wrong_version(status, &body, "1");
	assert_missing_key(services, &user_id, "01", &room, "alias").await;

	services
		.key_backups
		.delete_backup(&user_id, "01")
		.await;

	Ok(())
}

async fn seeded_backup(
	services: &Services,
	user_id: &UserId,
	room: &RoomId,
	session: &str,
	algorithm: &Raw<BackupAlgorithm>,
	key: &Raw<KeyBackupData>,
) -> Result<String> {
	let version = services
		.key_backups
		.create_backup(user_id, algorithm)
		.await?;

	add_key(services, user_id, &version, room, session, key).await?;

	Ok(version)
}

async fn add_key(
	services: &Services,
	user_id: &UserId,
	version: &str,
	room: &RoomId,
	session: &str,
	key: &Raw<KeyBackupData>,
) -> Result<(usize, u64)> {
	let keys = once((room, session, key)).stream();

	services
		.key_backups
		.add_keys(user_id, version, keys)
		.await
}

fn paused_keys<'a>(
	first: BackupKey<'a>,
	second: Option<BackupKey<'a>>,
) -> (impl Stream<Item = BackupKey<'a>> + Send + 'a, Receiver<()>, Sender<()>) {
	let (reached_tx, reached_rx) = channel();
	let (release_tx, release_rx) = channel();
	let tail = once_stream(async move {
		reached_tx
			.send(())
			.expect("pause receiver disappeared");

		release_rx
			.await
			.expect("pause sender disappeared");

		second
	})
	.filter_map(ready);

	let keys = iter([first]).chain(tail);

	(keys, reached_rx, release_tx)
}

async fn wait_paused<F: Future>(upload: F, reached: Receiver<()>) -> OwnedFuture<F> {
	let upload = Box::pin(upload); // cancellation ownership

	match select(upload, reached).await {
		| Either::Left(_) => panic!("operation finished before reaching the production pause"),
		| Either::Right((signal, upload)) => {
			signal.expect("production pause signal was canceled");

			upload
		},
	}
}

async fn pending_once<F: Future>(future: F, message: &str) -> OwnedFuture<F> {
	let mut future = Box::pin(future); // cancellation ownership

	poll_fn(|cx| {
		assert!(matches!(future.as_mut().poll(cx), Poll::Pending), "{message}");

		Poll::Ready(())
	})
	.await;

	future
}

async fn watch<F: Future>(future: F, message: &str) -> F::Output {
	timeout(Duration::from_secs(10), future)
		.await
		.unwrap_or_else(|_| panic!("{message}"))
}

async fn put_json(
	client: &Client<'_>,
	path: &str,
	version: &str,
	body: &Value,
) -> Result<(StatusCode, Value)> {
	let response = client
		.services
		.client
		.clients
		.default
		.put(client.url(path))
		.bearer_auth(client.token)
		.query(&[("version", version)])
		.json(body)
		.send()
		.await?;

	let status = response.status();
	let body = response.json().await?;

	Ok((status, body))
}

fn bulk_body(room: &RoomId, entries: &[(&str, &Raw<KeyBackupData>)]) -> Value {
	let rooms = BTreeMap::from([(room.as_str(), room_body(entries))]);

	json!({ "rooms": rooms })
}

fn room_body(entries: &[(&str, &Raw<KeyBackupData>)]) -> Value {
	let sessions = entries
		.iter()
		.copied()
		.collect::<BTreeMap<_, _>>();

	json!({ "sessions": sessions })
}

fn raw_value<T>(raw: &Raw<T>) -> Value {
	serde_json::from_str(raw.json().get()).expect("valid raw JSON")
}

fn assert_wrong_version(status: StatusCode, body: &Value, current: &str) {
	assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
	assert_eq!(body["errcode"], "M_WRONG_ROOM_KEYS_VERSION", "{body}");
	assert_eq!(body["current_version"], current, "{body}");
}

fn assert_not_found(status: StatusCode, body: &Value) {
	assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
	assert_eq!(body["errcode"], "M_NOT_FOUND", "{body}");
}

async fn assert_ciphertext(
	services: &Services,
	user_id: &UserId,
	version: &str,
	room: &RoomId,
	session: &str,
	expected: &str,
) -> Result {
	let key = services
		.key_backups
		.get_session(user_id, version, room, session)
		.await?;

	let value = raw_value(&key);

	assert_eq!(value["session_data"]["ciphertext"], expected);

	Ok(())
}

async fn assert_algorithm(
	services: &Services,
	user_id: &UserId,
	version: &str,
	expected: &str,
) -> Result {
	let algorithm = services
		.key_backups
		.get_backup(user_id, version)
		.await?;

	let value = raw_value(&algorithm);

	assert_eq!(value["auth_data"]["public_key"], expected);

	Ok(())
}

async fn assert_missing_backup(services: &Services, user_id: &UserId, version: &str) {
	let algorithm = services
		.key_backups
		.get_backup(user_id, version)
		.await;

	assert!(algorithm.is_err(), "deleted algorithm remains");

	let etag = services
		.key_backups
		.get_etag(user_id, version)
		.await;

	assert!(etag.is_err(), "deleted etag remains");

	let count = services
		.key_backups
		.count_keys(user_id, version)
		.await;

	assert_eq!(count, 0, "deleted keys remain");
}

async fn assert_missing_key(
	services: &Services,
	user_id: &UserId,
	version: &str,
	room: &RoomId,
	session: &str,
) {
	let key = services
		.key_backups
		.get_session(user_id, version, room, session)
		.await;

	assert!(key.is_err(), "unexpected key remains");
}

fn direct_user(services: &Services, localpart: &str) -> Result<OwnedUserId> {
	UserId::parse_with_server_name(localpart, services.globals.server_name()).map_err(Into::into)
}

fn room(localpart: &str) -> Result<OwnedRoomId> {
	RoomId::parse(format!("!{localpart}:example.com")).map_err(Into::into)
}

fn seed_metadata(
	services: &Services,
	user_id: &UserId,
	version: &str,
	algorithm: &Raw<BackupAlgorithm>,
	etag: u64,
) {
	services.db["backupid_algorithm"].put((user_id, version), Json(algorithm));
	services.db["backupid_etag"].put((user_id, version), etag);
}

fn seed_key(
	services: &Services,
	user_id: &UserId,
	version: &str,
	room: &RoomId,
	session: &str,
	key: &Raw<KeyBackupData>,
) {
	services.db["backupkeyid_backup"].put((user_id, version, room, session), Json(key));
}

fn algorithm(public_key: &str) -> Raw<BackupAlgorithm> {
	let json = json!({
		"algorithm": "m.megolm_backup.v1.curve25519-aes-sha2",
		"auth_data": {
			"public_key": public_key,
			"signatures": {}
		}
	})
	.to_string();

	Raw::from_json_string(json).expect("valid backup algorithm JSON")
}

fn key_data(
	ciphertext: &str,
	is_verified: bool,
	first_message_index: u64,
	forwarded_count: u64,
) -> Raw<KeyBackupData> {
	let json = json!({
		"first_message_index": first_message_index,
		"forwarded_count": forwarded_count,
		"is_verified": is_verified,
		"session_data": {
			"ciphertext": ciphertext,
			"ephemeral": "ephemeral",
			"mac": "mac"
		}
	})
	.to_string();

	Raw::from_json_string(json).expect("valid backup key JSON")
}

fn malformed_key(ciphertext: &str) -> Raw<KeyBackupData> {
	let json = json!({
		"first_message_index": 0,
		"forwarded_count": 0,
		"session_data": {
			"ciphertext": ciphertext,
			"ephemeral": "ephemeral",
			"mac": "mac"
		}
	})
	.to_string();

	Raw::from_json_string(json).expect("valid raw backup key JSON")
}
