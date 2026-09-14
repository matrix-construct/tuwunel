#![cfg(test)]

use std::{fs::remove_dir_all, iter::once, net::TcpListener};

use futures::future::join;
use reqwest::StatusCode;
use serde_json::{Value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result,
	ruma::{
		RoomId, UserId,
		api::client::backup::{BackupAlgorithm, KeyBackupData},
		serde::Raw,
	},
	utils::stream::IterStream,
};
use tuwunel_service::Services;

use self::client::{Client, field, register, wait_until_ready};

mod client;

const ACCESS_TOKEN: &str = "backup-delete-test-access-token-00";

#[test]
fn backup_delete_validates_metadata_and_advances_etags() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let db_path = Args::test_database_path("backup-delete");
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
	drop(remove_dir_all(&db_path));

	result
}

async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;

	let user_id = register(services, "backup_delete", ACCESS_TOKEN).await?;
	let user: &UserId = &user_id;
	let client = Client { services, base, token: ACCESS_TOKEN };
	let room_a = client.create_room(&json!({})).await?;
	let room_b = client.create_room(&json!({})).await?;
	let room_c = client.create_room(&json!({})).await?;
	let algorithm = algorithm();
	let key = key_data();

	let old = services
		.key_backups
		.create_backup(user, &algorithm)
		.await?;

	for (room, session) in [
		(room_a.as_ref(), "a1"),
		(room_a.as_ref(), "a10"),
		(room_b.as_ref(), "b1"),
		(room_b.as_ref(), "b2"),
		(room_c.as_ref(), "c1"),
	] {
		add_key(services, user, &old, room, session, &key).await?;
	}

	let latest = services
		.key_backups
		.create_backup(user, &algorithm)
		.await?;

	add_key(services, user, &latest, &room_a, "latest", &key).await?;

	assert_eq!(
		services
			.key_backups
			.get_latest_backup_version(user)
			.await?,
		latest,
		"latest backup version changed",
	);

	let (status, body) =
		put(&client, &format!("room_keys/keys/{room_c}/stale"), &old, &key).await?;

	assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
	assert_eq!(body["errcode"], "M_WRONG_ROOM_KEYS_VERSION", "{body}");
	assert_eq!(body["current_version"], latest, "{body}");

	let initial_etag = services
		.key_backups
		.get_etag(user, &old)
		.await?
		.to_string();

	let session_etag =
		delete_ok(&client, &format!("room_keys/keys/{room_a}/a1"), &old, 4).await?;

	assert_advanced(&initial_etag, &session_etag);
	assert!(
		services
			.key_backups
			.get_session(user, &old, &room_a, "a1")
			.await
			.is_err(),
		"deleted session remains",
	);

	services
		.key_backups
		.get_session(user, &old, &room_a, "a10")
		.await?;

	services
		.key_backups
		.get_session(user, &old, &room_b, "b1")
		.await?;

	let room_etag = delete_ok(&client, &format!("room_keys/keys/{room_b}"), &old, 2).await?;
	assert_advanced(&session_etag, &room_etag);
	for session in ["b1", "b2"] {
		assert!(
			services
				.key_backups
				.get_session(user, &old, &room_b, session)
				.await
				.is_err(),
			"deleted room session {session} remains",
		);
	}

	services
		.key_backups
		.get_session(user, &old, &room_a, "a10")
		.await?;

	services
		.key_backups
		.get_session(user, &old, &room_c, "c1")
		.await?;

	let final_etag = delete_ok(&client, "room_keys/keys", &old, 0).await?;
	assert_advanced(&room_etag, &final_etag);
	services
		.key_backups
		.get_session(user, &latest, &room_a, "latest")
		.await?;

	let missing_algorithm =
		seeded_backup(services, user, room_a.as_ref(), "missing-algorithm", &algorithm, &key)
			.await?;

	let original_etag = services
		.key_backups
		.get_etag(user, &missing_algorithm)
		.await?;

	services.db["backupid_algorithm"].del((user, missing_algorithm.as_str()));

	let (status, body) = delete(&client, "room_keys/keys", &missing_algorithm).await?;

	assert_not_found(status, &body);

	let remaining_keys = services
		.key_backups
		.count_keys(user, &missing_algorithm)
		.await;

	assert_eq!(remaining_keys, 1, "failed DELETE removed keys");

	let failed_etag = services
		.key_backups
		.get_etag(user, &missing_algorithm)
		.await?;

	assert_eq!(failed_etag, original_etag, "failed DELETE changed the etag");

	let missing_etag =
		seeded_backup(services, user, room_c.as_ref(), "missing-etag", &algorithm, &key).await?;

	services.db["backupid_etag"].del((user, missing_etag.as_str()));

	let path = format!("room_keys/keys/{room_c}/missing-etag");
	let (status, body) = delete(&client, &path, &missing_etag).await?;

	assert_not_found(status, &body);

	let remaining_keys = services
		.key_backups
		.count_keys(user, &missing_etag)
		.await;

	assert_eq!(remaining_keys, 1, "failed DELETE removed keys");

	let missing_etag_result = services
		.key_backups
		.get_etag(user, &missing_etag)
		.await;

	assert!(missing_etag_result.is_err(), "failed DELETE recreated the missing etag");

	Ok(())
}

async fn seeded_backup(
	services: &Services,
	user: &UserId,
	room: &RoomId,
	session: &str,
	algorithm: &Raw<BackupAlgorithm>,
	key: &Raw<KeyBackupData>,
) -> Result<String> {
	let version = services
		.key_backups
		.create_backup(user, algorithm)
		.await?;

	add_key(services, user, &version, room, session, key).await?;

	Ok(version)
}

async fn add_key(
	services: &Services,
	user: &UserId,
	version: &str,
	room: &RoomId,
	session: &str,
	key: &Raw<KeyBackupData>,
) -> Result {
	let keys = once((room, session, key)).stream();

	services
		.key_backups
		.add_keys(user, version, keys)
		.await
		.map(drop)
}

async fn delete(client: &Client<'_>, path: &str, version: &str) -> Result<(StatusCode, Value)> {
	let response = client
		.services
		.client
		.clients
		.default
		.delete(client.url(path))
		.bearer_auth(client.token)
		.query(&[("version", version)])
		.send()
		.await?;

	let status = response.status();
	let body = response.json().await?;

	Ok((status, body))
}

async fn put(
	client: &Client<'_>,
	path: &str,
	version: &str,
	key: &Raw<KeyBackupData>,
) -> Result<(StatusCode, Value)> {
	let response = client
		.services
		.client
		.clients
		.default
		.put(client.url(path))
		.bearer_auth(client.token)
		.query(&[("version", version)])
		.json(key)
		.send()
		.await?;

	let status = response.status();
	let body = response.json().await?;

	Ok((status, body))
}

async fn delete_ok(
	client: &Client<'_>,
	path: &str,
	version: &str,
	expected_count: u64,
) -> Result<String> {
	let (status, body) = delete(client, path, version).await?;

	assert_eq!(status, StatusCode::OK, "{body}");
	assert_eq!(body["count"], json!(expected_count), "{body}");

	Ok(field(&body, "etag")?.to_owned())
}

fn assert_advanced(before: &str, after: &str) {
	let before = before.parse::<u64>().expect("numeric prior etag");
	let after = after.parse::<u64>().expect("numeric new etag");

	assert!(after > before, "etag did not advance: {before} to {after}");
}

fn assert_not_found(status: StatusCode, body: &Value) {
	assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
	assert_eq!(body["errcode"], "M_NOT_FOUND", "{body}");
}

fn algorithm() -> Raw<BackupAlgorithm> {
	let json = json!({
		"algorithm": "m.megolm_backup.v1.curve25519-aes-sha2",
		"auth_data": {
			"public_key": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
			"signatures": {}
		}
	})
	.to_string();

	Raw::from_json_string(json).expect("valid backup algorithm JSON")
}

fn key_data() -> Raw<KeyBackupData> {
	let json = json!({
		"first_message_index": 0,
		"forwarded_count": 0,
		"is_verified": true,
		"session_data": {
			"ciphertext": "ciphertext",
			"ephemeral": "ephemeral",
			"mac": "mac"
		}
	})
	.to_string();

	Raw::from_json_string(json).expect("valid backup key JSON")
}
