use std::{collections::BTreeMap, iter::once, sync::Arc};

use futures::{StreamExt, TryStreamExt, future::join, pin_mut, poll};
use ruma::{
	OneTimeKeyAlgorithm, OneTimeKeyId, OwnedOneTimeKeyId, UInt, UserId, device_id,
	encryption::OneTimeKey,
	serde::{Base64, Raw, base64::Standard},
	signatures::{Ed25519KeyPair, KeyPair, to_canonical_json_string_for_signing},
	user_id,
};
use serde_json::{Value, from_value, json};
use tokio::sync::Notify;
use tuwunel_core::{Result, config::Figment, utils::to_canonical_object};
use tuwunel_database::{Cbor, Deserialized, Json, Map, serialize_key};

use super::{PASSWORD_SENTINEL, Service, device::DeviceLock};
use crate::{
	sync::{Connection, into_connection_key},
	test_utils::{create_devices, fixture, released_after, released_by},
};

const OLD_PASSWORD: &str = "old password";
const NEW_PASSWORD: &str = "new password";

#[tokio::test]
async fn deactivation_before_password_change_rejects_write() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let users = &fixture.services.users;
	let user_id = user_id!("@deactivation-first:localhost");

	users
		.create(user_id, Some(OLD_PASSWORD), None)
		.await?;

	let password_guard = users.changing_password.lock(user_id).await;
	let password_clear = users.set_password(user_id, None);
	let password_change = users.set_password_if_active(user_id, Some(NEW_PASSWORD));

	pin_mut!(password_clear);
	pin_mut!(password_change);
	assert!(poll!(password_clear.as_mut()).is_pending());
	assert!(poll!(password_change.as_mut()).is_pending());

	drop(password_guard);

	password_clear.await?;
	assert!(password_change.await.is_err());
	assert!(users.is_deactivated(user_id).await?);

	Ok(())
}

#[tokio::test]
async fn password_change_before_deactivation_leaves_disabled() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let users = &fixture.services.users;
	let user_id = user_id!("@password-first:localhost");

	users
		.create(user_id, Some(OLD_PASSWORD), None)
		.await?;

	let checked = Notify::new();
	let release = Notify::new();
	let after_check = async {
		checked.notify_one();
		release.notified().await;
	};

	let password_change =
		users.set_password_if_active_after(user_id, Some(NEW_PASSWORD), after_check);

	let deactivate = async {
		checked.notified().await;

		let password_clear = users.set_password(user_id, None);

		pin_mut!(password_clear);
		assert!(poll!(password_clear.as_mut()).is_pending());
		release.notify_one();

		password_clear.await
	};

	let (password_change, password_clear) = join(password_change, deactivate).await;

	password_change?;
	password_clear?;
	assert!(users.is_deactivated(user_id).await?);

	Ok(())
}

#[tokio::test]
async fn password_locks_are_independent_and_cancel_safe() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let users = &fixture.services.users;
	let user_a = user_id!("@password-lock-a:localhost");
	let user_b = user_id!("@password-lock-b:localhost");

	users
		.create(user_a, Some(OLD_PASSWORD), None)
		.await?;

	users
		.create(user_b, Some(OLD_PASSWORD), None)
		.await?;

	let password_guard = users.changing_password.lock(user_a).await;

	// owned pin: drop(queued) cancels it
	let mut queued = Box::pin(users.set_password_if_active(user_a, Some(NEW_PASSWORD)));

	assert!(poll!(queued.as_mut()).is_pending());
	drop(queued);

	users
		.set_password_if_active(user_b, Some(NEW_PASSWORD))
		.await?;

	drop(password_guard);

	users
		.set_password_if_active(user_a, Some(NEW_PASSWORD))
		.await?;

	Ok(())
}

#[tokio::test]
async fn rejected_password_change_releases_lock() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let users = &fixture.services.users;
	let user_id = user_id!("@password-origin:localhost");

	users
		.create(user_id, Some(PASSWORD_SENTINEL), Some("ldap"))
		.await?;

	assert!(
		users
			.set_password_if_active(user_id, Some(NEW_PASSWORD))
			.await
			.is_err()
	);

	users.set_password(user_id, None).await?;
	assert!(users.is_deactivated(user_id).await?);

	Ok(())
}

#[tokio::test]
async fn unused_fallback_keys_stay_with_their_device() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let users = &fixture.services.users;
	let user_id = user_id!("@fallback:localhost");
	let device_id = device_id!("FALLBACK");
	let longer_id = device_id!("FALLBACK2");

	create_devices(users, user_id, &[device_id, longer_id]).await?;

	let key_id = <&OneTimeKeyId>::try_from("signed_curve25519:AAAAAQ")?;
	let key: Raw<OneTimeKey> = from_value(json!("fallback key"))?;

	users
		.add_fallback_keys(user_id, longer_id, once((key_id, &key)))
		.await?;

	let unused = async |device_id| {
		users
			.unused_fallback_key_algorithms(user_id, device_id)
			.collect::<Vec<_>>()
			.await
	};

	assert_eq!(unused(device_id).await, []);
	assert_eq!(unused(longer_id).await, [OneTimeKeyAlgorithm::SignedCurve25519]);

	users
		.take_fallback_key(user_id, longer_id, &OneTimeKeyAlgorithm::SignedCurve25519)
		.await?;

	assert_eq!(unused(longer_id).await, []);

	Ok(())
}

/// A pool put back from an older backup issues again a key a claim has already
/// taken. Once cleared it issues none: every device's count reads zero, the
/// fallback key still answers, and a fresh upload is taken.
#[tokio::test]
async fn cleared_pool_issues_no_restored_key() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let users = &fixture.services.users;
	let alice = user_id!("@alice:localhost");
	let bob = user_id!("@bob:localhost");
	let (phone, laptop, tablet) =
		(device_id!("PHONE"), device_id!("LAPTOP"), device_id!("TABLET"));
	let devices = [(alice, phone), (alice, laptop), (bob, tablet)];
	let curve25519 = OneTimeKeyAlgorithm::from("curve25519");

	create_devices(users, alice, &[phone, laptop]).await?;
	create_devices(users, bob, &[tablet]).await?;

	for (user_id, device_id) in devices {
		users
			.add_one_time_keys(user_id, device_id, &one_time_keys(&["AAAAAA", "AAAAAQ"])?, 256)
			.await?;
	}

	let fallback_id = <&OneTimeKeyId>::try_from("curve25519:fallback")?;
	let fallback: Raw<OneTimeKey> = from_value(json!("fallback key"))?;

	users
		.add_fallback_keys(alice, phone, once((fallback_id, &fallback)))
		.await?;

	let otk = pool(users);
	let backup = rows(otk).await?;
	let (claimed, _) = users
		.take_one_time_key(alice, phone, &curve25519)
		.await?;

	restore(otk, &backup);
	let (again, _) = users
		.take_one_time_key(alice, phone, &curve25519)
		.await?;

	assert_eq!(again, claimed, "a restored pool issues a claimed key again");

	restore(otk, &backup);
	let marked = users.last_one_time_keys_update(alice).await;

	assert_eq!(users.clear_one_time_keys(None).await?, (6, 3));
	assert!(users.last_one_time_keys_update(alice).await > marked);

	for (user_id, device_id) in devices {
		let counts = users
			.count_one_time_keys(user_id, device_id)
			.await;

		assert!(counts.values().all(|&count| count == UInt::MIN), "{device_id}: {counts:?}");
	}

	users
		.take_one_time_key(alice, phone, &curve25519)
		.await
		.expect_err("a cleared pool issues no one-time key");

	let (served, _) = users
		.take_fallback_key(alice, phone, &curve25519)
		.await?;

	assert_eq!(served, fallback_id);

	users
		.add_one_time_keys(alice, phone, &one_time_keys(&["AAAAAg"])?, 256)
		.await?;

	let (fresh, _) = users
		.take_one_time_key(alice, phone, &curve25519)
		.await?;

	assert_eq!(fresh.as_str(), "curve25519:AAAAAg");

	Ok(())
}

/// Clearing one user's keys leaves everyone else's, and takes the rows of a
/// device that is gone with the rest.
#[tokio::test]
async fn clearing_one_users_keys_leaves_the_others() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let users = &fixture.services.users;
	let alice = user_id!("@alice:localhost");
	let bob = user_id!("@bob:localhost");
	let (phone, tablet, watch) = (device_id!("PHONE"), device_id!("TABLET"), device_id!("WATCH"));
	let curve25519 = OneTimeKeyAlgorithm::from("curve25519");

	create_devices(users, alice, &[phone]).await?;
	create_devices(users, bob, &[tablet, watch]).await?;

	for (user_id, device_id) in [(alice, phone), (bob, tablet), (bob, watch)] {
		users
			.add_one_time_keys(user_id, device_id, &one_time_keys(&["AAAAAA", "AAAAAQ"])?, 256)
			.await?;
	}

	let otk = pool(users);
	let backup = rows(otk).await?;

	users.remove_device(bob, watch).await;
	restore(otk, &backup);

	assert_eq!(users.clear_one_time_keys(Some(bob)).await?, (4, 2));
	assert_eq!(rows(otk).await?.len(), 2);

	let counts = users.count_one_time_keys(alice, phone).await;

	assert_eq!(counts.get(&curve25519), Some(&UInt::from(2_u32)));

	Ok(())
}

/// A row that does not read as a key goes with the rest and is counted, so a
/// clearing never reports a pool empty that still holds a row.
#[tokio::test]
async fn a_row_that_does_not_read_as_a_key_is_cleared_too() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let users = &fixture.services.users;
	let alice = user_id!("@alice:localhost");
	let phone = device_id!("PHONE");

	create_devices(users, alice, &[phone]).await?;
	users
		.add_one_time_keys(alice, phone, &one_time_keys(&["AAAAAA"])?, 256)
		.await?;

	let otk = pool(users);

	otk.insert(b"not a key row", b"one-time key");

	assert_eq!(users.clear_one_time_keys(None).await?, (2, 1));
	assert!(rows(otk).await?.is_empty());

	Ok(())
}

#[tokio::test]
async fn signature_merge_waits_for_the_key_row() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let (users, keyid_key) = (&services.users, &services.db["keyid_key"]);
	let user_id = user_id!("@signing:localhost");
	let device_id = device_id!("SIGNER");
	let signing_key_id = "ed25519:SIGNER";

	create_devices(users, user_id, &[device_id]).await?;

	let signer = key_pair("SIGNER");
	let root_key = public_key(&key_pair("ROOT"));
	let root_row = (user_id, root_key.as_str());
	let root = cross_signing_key(user_id, "master", &root_key);
	let device = json!({"user_id": user_id, "device_id": device_id,
		"keys": {signing_key_id: public_key(&signer)}});

	keyid_key.put(root_row, Json(&root));
	keyid_key.put((user_id, device_id), Json(device));
	services.db["userid_masterkeyid"].insert(user_id, serialize_key(root_row)?);

	let canonical = to_canonical_json_string_for_signing(&to_canonical_object(&root)?)?;
	let signature = signer.sign(canonical.as_bytes()).base64();
	let signatures = [(signing_key_id.to_owned(), signature.clone())].into();
	let other = user_id!("@other:localhost");
	let other_signature = json!({other: {"ed25519:OTHER": "other signature"}});
	let signed_root = signed_cross_signing_key(user_id, "master", &root_key, &other_signature);
	let row_lock = lock_key_row(users, root_row).await;
	let merged_meanwhile = async { keyid_key.put(root_row, Json(&signed_root)) };
	let merge = users.sign_key(user_id, &root_key, signatures, user_id);

	released_after(row_lock, merged_meanwhile, merge).await?;

	let stored: Value = keyid_key.qry(&root_row).await.deserialized()?;

	assert_eq!(stored["signatures"][user_id.as_str()][signing_key_id], signature);
	assert_eq!(stored["signatures"][other.as_str()]["ed25519:OTHER"], "other signature");

	Ok(())
}

#[tokio::test]
async fn device_key_upload_holds_the_device_lock() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let (users, keyid_key) = (&services.users, &services.db["keyid_key"]);
	let user_id = user_id!("@device-keys:localhost");
	let (device_id, removed_id) = (device_id!("UPLOADER"), device_id!("REMOVED"));
	let keys =
		|device_id| from_value(json!({"user_id": user_id, "device_id": device_id, "keys": {}}));

	create_devices(users, user_id, &[device_id, removed_id]).await?;

	let device_lock = users.lock_device(user_id, device_id).await;

	released_by(device_lock, users.add_device_keys(user_id, device_id, &keys(device_id)?))
		.await?;

	assert!(keyid_key.contains(&(user_id, device_id)).await);

	let device_lock = users.lock_device(user_id, removed_id).await;
	let removed_meanwhile =
		async { services.db["userdeviceid_metadata"].del((user_id, removed_id)) };

	let removed_keys = keys(removed_id)?;
	let upload = users.add_device_keys(user_id, removed_id, &removed_keys);

	released_after(device_lock, removed_meanwhile, upload).await?;

	assert!(!keyid_key.contains(&(user_id, removed_id)).await);

	Ok(())
}

#[tokio::test]
async fn cross_signing_upload_waits_for_its_key_rows() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let (users, keyid_key) = (&services.users, &services.db["keyid_key"]);
	let user_id = user_id!("@cross-signing:localhost");
	let key_ids = ["ROOT", "SELF", "USER"].map(|version| public_key(&key_pair(version)));
	let [root_key, self_key, user_key] = &key_ids;
	let key = |usage, key_id: &str| from_value(cross_signing_key(user_id, usage, key_id));
	let root = Some(key("master", root_key)?);
	let self_signing = Some(key("self_signing", self_key)?);
	let user_signing = Some(key("user_signing", user_key)?);

	users.create(user_id, None, None).await?;

	let row_lock = lock_key_row(users, (user_id, self_key)).await;
	let unwritten_meanwhile = async {
		for key_id in &key_ids {
			assert!(!keyid_key.contains(&(user_id, key_id)).await);
			assert_eq!(
				users
					.is_cross_signing_key_id(user_id, key_id)
					.await
					.ok(),
				Some(false)
			);
		}
	};

	let upload =
		users.add_cross_signing_keys(user_id, &root, &self_signing, &user_signing, false);

	released_after(row_lock, unwritten_meanwhile, upload).await?;

	for key_id in &key_ids {
		assert!(keyid_key.contains(&(user_id, key_id)).await);
		assert!(
			users
				.is_cross_signing_key_id(user_id, key_id)
				.await?
		);
	}

	Ok(())
}

#[tokio::test]
async fn remote_master_key_keeps_our_signatures() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let (users, keyid_key) = (&services.users, &services.db["keyid_key"]);
	let user_id = user_id!("@remote:example.org");
	let (ours, other) = (user_id!("@ours:localhost"), user_id!("@other:localhost"));
	let root_key = public_key(&key_pair("REMOTE"));
	let root_row = (user_id, root_key.as_str());
	let root = |signatures| signed_cross_signing_key(user_id, "master", &root_key, &signatures);

	keyid_key.put(root_row, Json(root(json!({ours: {"ed25519:OURS": "ours"}}))));

	let remote = from_value(root(json!({user_id: {"ed25519:REMOTE": "theirs"}})))?;
	let row_lock = lock_key_row(users, root_row).await;

	let signed_meanwhile = async {
		let signatures = json!({ours: {"ed25519:OURS": "ours"},
			other: {"ed25519:OTHER": "other"}, user_id: {"ed25519:OLD": "old"}});

		keyid_key.put(root_row, Json(root(signatures)));
	};

	let store = users.store_remote_master_key(user_id, &remote);

	released_after(row_lock, signed_meanwhile, store).await?;

	let stored: Value = keyid_key.qry(&root_row).await.deserialized()?;
	let expected = [
		(user_id, "ed25519:REMOTE", "theirs"),
		(user_id, "ed25519:OLD", "old"),
		(ours, "ed25519:OURS", "ours"),
		(other, "ed25519:OTHER", "other"),
	];

	for (signer, key_id, signature) in expected {
		assert_eq!(stored["signatures"][signer.as_str()][key_id], signature);
	}

	assert!(
		users
			.is_cross_signing_key_id(user_id, &root_key)
			.await?
	);

	Ok(())
}

#[tokio::test]
async fn device_removal_keeps_its_connections() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let (users, sync) = (&services.users, &services.sync);
	let user_id = user_id!("@conn:localhost");
	let device_id = device_id!("D");
	let key = into_connection_key(user_id, Some(device_id), Some("room-list"));
	let next_batch = 42;

	create_devices(users, user_id, &[device_id]).await?;

	let conn = sync.load_or_init_connection(&key).await?;

	conn.lock().await.next_batch = next_batch;
	sync.store_connection(&key, &conn, &*conn.lock().await)
		.await?;

	users.remove_device(user_id, device_id).await;

	assert!(sync.is_connection_stored(&key).await);

	users
		.create_device(user_id, Some(device_id), (None, None), None, None, None)
		.await?;

	let resumed = sync.load_or_init_connection(&key).await?;

	assert_eq!(resumed.lock().await.next_batch, next_batch);

	let Cbor(stored): Cbor<Connection> = services.db["userdeviceconnid_conn"]
		.qry(&key)
		.await
		.deserialized()?;

	assert_eq!(stored.next_batch, next_batch);

	Ok(())
}

fn key_pair(version: &str) -> Ed25519KeyPair {
	Ed25519KeyPair::from_der(&Ed25519KeyPair::generate(), version.into())
		.expect("generated key pair should parse")
}

fn public_key(key_pair: &Ed25519KeyPair) -> String {
	Base64::<Standard, _>::new(key_pair.public_key()).encode()
}

fn cross_signing_key(user_id: &UserId, usage: &str, key_id: &str) -> Value {
	signed_cross_signing_key(user_id, usage, key_id, &json!({}))
}

fn signed_cross_signing_key(
	user_id: &UserId,
	usage: &str,
	key_id: &str,
	signatures: &Value,
) -> Value {
	json!({"user_id": user_id, "usage": [usage],
		"keys": {format!("ed25519:{key_id}"): key_id}, "signatures": signatures})
}

async fn lock_key_row(users: &Service, (user_id, key_id): (&UserId, &str)) -> DeviceLock {
	// Key rows share the device lock, keyed by key ID.
	users.lock_device(user_id, key_id.into()).await
}

fn one_time_keys(names: &[&str]) -> Result<BTreeMap<OwnedOneTimeKeyId, Raw<OneTimeKey>>> {
	names
		.iter()
		.map(|name| -> Result<_> {
			let id = OwnedOneTimeKeyId::try_from(format!("curve25519:{name}"))?;

			Ok((id, from_value(json!("one-time key"))?))
		})
		.collect()
}

/// The one-time-key pool, which a backup copies and a restore puts back.
fn pool(users: &Service) -> &Arc<Map> {
	users
		.db
		.onetimekeyid4225_otk
		.as_ref()
		.expect("the one-time-key column should be open")
}

async fn rows(map: &Arc<Map>) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
	map.raw_stream()
		.map_ok(|(key, val)| (key.to_vec(), val.to_vec()))
		.try_collect()
		.await
}

/// Puts rows back as a restore from a backup does.
fn restore(map: &Map, rows: &[(Vec<u8>, Vec<u8>)]) {
	for (key, val) in rows {
		map.insert(key, val);
	}
}
