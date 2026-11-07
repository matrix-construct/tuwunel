use std::{array::from_fn, sync::Arc};

use ruma::{UserId, room_id, user_id};
use serde_json::json;
use tuwunel_core::{Result, config::Figment, itertools::Itertools, utils::hash::password};
use tuwunel_database::{Json, Map, serialize_key};

use super::{Summary, checks, find};
use crate::{
	Services,
	migrations::{DATABASE_VERSION, MARKERS, fresh, marker_present, migrate},
	test_utils::fixture,
	users::PASSWORD_SENTINEL,
};

#[test]
fn check_names_are_unique() {
	let names = checks().map(|check| check.name);

	assert_eq!(names.clone().count(), names.sorted_unstable().dedup().count());
}

#[tokio::test]
async fn fresh_database_is_clean() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;

	fresh(services).await?;

	for check in checks().filter(|check| check.runnable(services)) {
		assert_eq!(count(services, check.name).await?, 0, "{}", check.name);
	}

	Ok(())
}

async fn count(services: &Services, name: &str) -> Result<u64> {
	Ok(run(services, name).await?.findings())
}

async fn run(services: &Services, name: &str) -> Result<Summary> {
	find(name)
		.expect("the check is registered")
		.run(services, 8)
		.await
}

#[tokio::test]
async fn schema_reports_an_unstamped_step() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;

	fresh(services).await?;
	services.db["global"].remove(MARKERS[0]);

	assert_eq!(count(services, "schema").await?, 1);

	Ok(())
}

#[tokio::test]
async fn schema_reports_a_foreign_version() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;

	fresh(services).await?;
	services
		.globals
		.db
		.bump_database_version(u64::MAX);

	let summary = run(services, "schema").await?;

	assert_eq!(summary.counts.get("schema-version"), Some(&1));

	Ok(())
}

#[tokio::test]
async fn schema_reports_a_missing_version() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;

	fresh(services).await?;
	services.db["global"].remove(b"version");

	let summary = run(services, "schema").await?;

	assert_eq!(summary.counts.get("missing-version"), Some(&1));

	Ok(())
}

#[tokio::test]
async fn schema_reports_a_malformed_version() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;

	let overlong = overlong(DATABASE_VERSION);

	fresh(services).await?;

	for version in [&b"bad"[..], &overlong] {
		services.db["global"].insert(b"version", version);

		let summary = run(services, "schema").await?;

		assert_eq!(summary.counts.get("malformed-version"), Some(&1), "{version:?}");
	}

	Ok(())
}

/// An id's eight bytes followed by one more, as no writer stores it.
///
/// Every short id and the schema version are exactly eight bytes, so a check must
/// report this as malformed rather than read its prefix.
fn overlong(id: u64) -> [u8; 9] {
	let bytes = id.to_be_bytes();

	from_fn(|index| bytes.get(index).copied().unwrap_or(0))
}

#[tokio::test]
async fn referencedevents_repair_clears_the_finding() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let referencedevents = &services.db["referencedevents"];

	fresh(services).await?;
	referencedevents.insert("!room:localhost$event", []);
	referencedevents.insert("no-sigil", []);

	let summary = run(services, "referenced-events").await?;

	assert_eq!(summary.counts.get("missing-separator"), Some(&1));
	assert_eq!(summary.counts.get("unparsable-key"), Some(&1));

	services.db["global"].remove("fix_referencedevents_missing_sep");
	migrate(services, false).await?;

	assert_eq!(count(services, "referenced-events").await?, 1);
	referencedevents
		.exists(b"!room:localhost\xFF$event".as_slice())
		.await?;

	Ok(())
}

#[tokio::test]
async fn joined_keys_repair_clears_the_finding() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let roomuserid_joined = &services.db["roomuserid_joined"];
	let doubled = b"!room:localhost\xFF\xFF@alice:localhost";
	let single = b"!room:localhost\xFF@alice:localhost";

	fresh(services).await?;
	roomuserid_joined.insert(doubled.as_slice(), b"count");
	roomuserid_joined.insert("no-separator", []);

	let summary = run(services, "joined-keys").await?;

	assert_eq!(summary.counts.get("doubled-separator"), Some(&1));
	assert_eq!(summary.counts.get("missing-separator"), Some(&1));

	services.db["global"].remove("fix_bad_double_separator_in_state_cache");
	migrate(services, false).await?;

	assert_eq!(count(services, "joined-keys").await?, 1);
	assert_eq!(&*roomuserid_joined.get(single.as_slice()).await?, b"count");
	assert!(!present(roomuserid_joined, doubled).await);

	Ok(())
}

async fn present(map: &Arc<Map>, key: &[u8]) -> bool { map.exists(key).await.is_ok() }

#[tokio::test]
async fn read_receipts_repair_keeps_threaded_rows() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let receipts = &services.db["readreceiptid_readreceipt"];
	let room = room_id!("!room:localhost");
	let (alice, bob) = (user_id!("@alice:localhost"), user_id!("@bob:localhost"));
	let unreadable = b"!room:localhost\xFFshort";
	let legacy = |count: u64, user: &UserId| serialize_key((room, count, user));
	let threaded = |count: u64, kind: &str| serialize_key((room, count, alice, kind));
	let repeated = legacy(2, alice)?;
	let bad_kind = [legacy(8, alice)?.as_slice(), b"\xFF\xC0"].concat();
	let kept = [
		legacy(1, alice)?,
		threaded(3, "main")?,
		threaded(4, "$root")?,
		legacy(5, bob)?,
		legacy(6, alice)?,
		legacy(u64::MAX, alice)?,
	];

	fresh(services).await?;
	receipts.insert(repeated.as_slice(), []);
	receipts.insert(unreadable.as_slice(), []);
	receipts.insert(&bad_kind, []);

	for key in &kept {
		receipts.insert(key, []);
	}

	let summary = run(services, "read-receipts").await?;

	assert_eq!(summary.counts.get("repeated-receipt"), Some(&1));
	assert_eq!(summary.counts.get("unparsable-key"), Some(&2));

	services.db["global"].remove("fix_readreceiptid_readreceipt_duplicates");
	migrate(services, false).await?;

	assert_eq!(count(services, "read-receipts").await?, 2);
	assert!(!present(receipts, &repeated).await);
	assert!(present(receipts, &bad_kind).await);

	for key in &kept {
		assert!(present(receipts, key).await, "{key:?}");
	}

	Ok(())
}

#[tokio::test]
async fn read_receipts_repair_keeps_a_shared_prefix_pair() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let receipts = &services.db["readreceiptid_readreceipt"];
	let room = room_id!("!room:localhost");
	let alice = user_id!("@alice:localhost");
	let legacy = serialize_key((room, 1_u64, alice))?;
	let threaded = serialize_key((room, 1_u64, alice, "main"))?;

	fresh(services).await?;
	receipts.insert(&legacy, []);
	receipts.insert(&threaded, []);

	assert_eq!(count(services, "read-receipts").await?, 0);

	services.db["global"].remove("fix_readreceiptid_readreceipt_duplicates");
	migrate(services, false).await?;

	assert!(present(receipts, &legacy).await);
	assert!(present(receipts, &threaded).await);

	Ok(())
}

#[tokio::test]
async fn sentinel_passwords_repair_clears_the_finding() -> Result {
	let config = Figment::new()
		.merge(("admin.room.create", false))
		.merge(("identity_provider.test.brand", "test"))
		.merge(("identity_provider.test.client_id", "test"))
		.merge(("identity_provider.test.client_secret", "test"))
		.merge(("identity_provider.test.issuer_url", "https://idp.localhost"));

	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let userid_password = &services.db["userid_password"];
	let marker = "fix_hashed_sentinel_passwords";
	let real = password("hunter2", services.config.password_hash_cost())?;

	fresh(services).await?;
	seed_passwords(services)?;
	userid_password.insert("@carol:localhost", &real);

	let summary = run(services, "sentinel-passwords").await?;

	assert_eq!(summary.counts.get("hashed-sentinel"), Some(&1));
	assert_eq!(summary.counts.get("unparsable-row"), Some(&1));

	services.db["global"].remove(marker);
	migrate(services, false).await?;

	assert!(marker_present(services, marker).await?);
	assert_eq!(count(services, "sentinel-passwords").await?, 1);
	assert_eq!(&*userid_password.get("@alice:localhost").await?, b"*");
	assert_eq!(&*userid_password.get("@carol:localhost").await?, real.as_bytes());

	Ok(())
}

fn seed_passwords(services: &Services) -> Result {
	let userid_password = &services.db["userid_password"];
	let hashed = password(PASSWORD_SENTINEL, services.config.password_hash_cost())?;

	userid_password.insert("@alice:localhost", &hashed);
	userid_password.insert("@bob:localhost", PASSWORD_SENTINEL);
	userid_password.insert("@dave:localhost", b"\xFF");
	userid_password.insert("@erin:localhost", []);

	Ok(())
}

#[tokio::test]
async fn sentinel_passwords_wait_for_a_provider() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let marker = "fix_hashed_sentinel_passwords";

	fresh(services).await?;
	seed_passwords(services)?;

	let summary = run(services, "sentinel-passwords").await?;

	assert_eq!(summary.counts.get("hashed-sentinel"), Some(&1));

	services.db["global"].remove(marker);
	migrate(services, false).await?;

	assert!(!marker_present(services, marker).await?);
	assert_eq!(count(services, "sentinel-passwords").await?, summary.findings());

	Ok(())
}

#[tokio::test]
async fn media_uploader_keys_repair_clears_the_finding() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let mediaid_user = &services.db["mediaid_user"];
	let legacy = b"mxc://localhost/legacy";
	let unowned = b"mxc://localhost/unowned";
	let upgraded = b"mxc://localhost/legacy\xFF@alice:localhost";
	let remote = b"mxc://example.org/legacy";

	fresh(services).await?;
	mediaid_user.insert(legacy.as_slice(), "@alice:localhost");
	mediaid_user.insert(unowned.as_slice(), "not a user");
	mediaid_user.insert(remote.as_slice(), "@alice:localhost");

	let summary = run(services, "media-uploader-keys").await?;

	assert_eq!(summary.counts.get("legacy-key"), Some(&2));
	assert_eq!(summary.counts.get("unowned-key"), Some(&1));
	assert_eq!(count(services, "remote-media-uploaders").await?, 0);

	services.db["global"].remove("upgrade_legacy_mediaid_user");
	services.db["global"].remove("remove_remote_media_userid");
	migrate(services, false).await?;

	assert_eq!(count(services, "media-uploader-keys").await?, 0);
	assert_eq!(count(services, "remote-media-uploaders").await?, 0);
	assert_eq!(&*mediaid_user.get(upgraded.as_slice()).await?, b"@alice:localhost");
	assert!(!present(mediaid_user, legacy).await);
	assert!(!present(mediaid_user, unowned).await);
	assert!(!present(mediaid_user, remote).await);

	Ok(())
}

#[tokio::test]
async fn remote_media_uploaders_repair_clears_the_finding() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let mediaid_user = &services.db["mediaid_user"];
	let local = b"mxc://localhost/local\xFF@alice:localhost";
	let foreign = b"mxc://example.org/media\xFF@alice:localhost";
	let invalid = b"not an mxc\xFF@alice:localhost";
	let bad_user = b"mxc://example.org/media\xFFnot a user";
	let extra = b"mxc://example.org/media\xFF@alice:localhost\xFFextra";

	fresh(services).await?;

	for key in [local.as_slice(), foreign, invalid, bad_user, extra] {
		mediaid_user.insert(key, "@alice:localhost");
	}

	let summary = run(services, "remote-media-uploaders").await?;

	assert_eq!(summary.counts.get("remote-media"), Some(&1));
	assert_eq!(summary.counts.get("invalid-mxc"), Some(&1));
	assert_eq!(summary.counts.get("unparsable-key"), Some(&2));

	services.db["global"].remove("remove_remote_media_userid");
	migrate(services, false).await?;

	assert_eq!(count(services, "remote-media-uploaders").await?, 2);
	assert!(!present(mediaid_user, foreign).await);
	assert!(!present(mediaid_user, invalid).await);

	for key in [local.as_slice(), bad_user, extra] {
		assert!(present(mediaid_user, key).await, "{key:?}");
	}

	Ok(())
}

#[tokio::test]
async fn profile_values_repair_clears_the_finding() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let values = &services.db["useridprofilekey_value"];
	let (alice, bob) = (user_id!("@alice:localhost"), user_id!("@bob:localhost"));

	fresh(services).await?;
	values.put_raw((alice, "bare"), b"bare text");
	values.put_raw((alice, "opaque"), b"\xFF\xFE");
	values.put((alice, "valid"), Json(json!({ "a": 1 })));
	services.db["userid_displayname"].insert(bob.as_str(), "Bob");

	let summary = run(services, "profile-values").await?;

	assert_eq!(summary.counts.get("bare-text"), Some(&1));
	assert_eq!(summary.counts.get("not-text"), Some(&1));

	services.db["global"].remove("migrate_profile_keys_to_useridprofilekey");
	migrate(services, false).await?;

	assert_eq!(count(services, "profile-values").await?, 0);
	assert_eq!(&*values.qry(&(alice, "bare")).await?, br#""bare text""#);
	assert_eq!(&*values.qry(&(bob, "displayname")).await?, br#""Bob""#);
	assert!(!present(values, &serialize_key((alice, "opaque"))?).await);
	assert!(present(values, &serialize_key((alice, "valid"))?).await);

	Ok(())
}
