use std::{array::from_fn, sync::Arc};

use futures::StreamExt;
use ruma::{EventId, RoomId, UserId, event_id, events::StateEventType, room_id, user_id};
use serde_json::{Value, json};
use tuwunel_core::{
	Result,
	config::Figment,
	itertools::Itertools,
	utils::{IterStream, hash::password},
};
use tuwunel_database::{Json, Map, serialize_key};
use tuwunel_matrix::{PduCount, PduId, RawPduId};

use super::{Summary, checks, find};
use crate::{
	Services,
	migrations::{DATABASE_VERSION, MARKERS, fresh, marker_present, migrate},
	rooms::{short::ShortRoomId, timeline::bias_count},
	test_utils::{compress_state, fixture, pdu_id, pdu_json, save_room_state, with_state_key},
	users::PASSWORD_SENTINEL,
};

type StateEvent<'a> = (u64, &'a EventId, &'a str, &'a str, Value);

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

	for check in checks() {
		assert_eq!(walk(services, check.name).await?.findings(), 0, "{}", check.name);
	}

	Ok(())
}

/// Runs a check past its refusal of a writable database.
///
/// The fixture's database is writable, but nothing else writes to it while a
/// test walks it, so a quiescent check still answers truly.
async fn walk(services: &Services, name: &str) -> Result<Summary> {
	let check = find(name).expect("the check is registered");

	(check.walk)(services, 8).await
}

#[tokio::test]
async fn quiescent_check_refuses_a_writable_database() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;

	fresh(services).await?;

	for check in checks().filter(|check| check.quiescent) {
		assert!(!check.runnable(services), "{}", check.name);
		assert!(run(services, check.name).await.is_err(), "{}", check.name);
	}

	Ok(())
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

#[tokio::test]
async fn timestamp_index_repair_clears_the_finding() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let pdus = &services.db["pduid_pdu"];
	let index = &services.db["roomid_tscount_pducount"];
	let room = room_id!("!room:localhost");
	let event = |ts: u64| json!({ "room_id": room, "origin_server_ts": ts });
	let mismatched = (room, 20_u64, bias_count(pdu_id(2).count()));

	fresh(services).await?;
	pdus.raw_put(pdu_id(1), Json(event(10)));
	pdus.raw_put(pdu_id(2), Json(event(20)));
	pdus.raw_put(pdu_id(3), Json(json!({})));
	pdus.insert(b"short", b"{}");
	index.put_raw(mismatched, pdu_id(9).count());

	let summary = walk(services, "timestamp-index").await?;

	assert_eq!(summary.counts.get("missing-row"), Some(&1));
	assert_eq!(summary.counts.get("mismatched-count"), Some(&1));
	assert_eq!(summary.counts.get("unparsable-key"), Some(&1));
	assert_eq!(summary.findings(), 3);

	services.db["global"].remove("rebuild_roomid_tscount_pducount");
	migrate(services, false).await?;

	let summary = walk(services, "timestamp-index").await?;

	assert_eq!(summary.counts.get("unparsable-key"), Some(&1));
	assert_eq!(summary.findings(), 1);
	assert_eq!(&*index.qry(&mismatched).await?, pdu_id(2).count().as_slice());

	Ok(())
}

#[tokio::test]
async fn relation_index_repair_clears_the_finding() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let index = &services.db["relatesto_typed"];
	let room = room_id!("!room:localhost");
	let alice = user_id!("@alice:localhost");
	let parent = event_id!("$parent:localhost");
	let edit = event_id!("$edit:localhost");
	let reference = event_id!("$reference:localhost");
	let (broken, orphan) = (event_id!("$broken:localhost"), event_id!("$orphan:localhost"));
	let undecodable = json!({ "room_id": room, "origin_server_ts": 1 });
	let message =
		|id: &EventId, content: Value| pdu_json(id, room, alice, "m.room.message", &content);

	let relation = |id: &EventId, rel_type: &str, parent: &EventId| {
		message(id, json!({ "m.relates_to": { "rel_type": rel_type, "event_id": parent } }))
	};

	fresh(services).await?;
	store(services, 1, 1, parent, &message(parent, json!({})));
	store(services, 1, 2, edit, &relation(edit, "m.replace", parent));
	store(services, 1, 4, reference, &relation(reference, "m.reference", parent));
	store(services, 1, 5, orphan, &relation(orphan, "m.replace", broken));
	services.db["eventid_pduid"].insert(broken.as_bytes(), b"bad");
	services.db["pduid_pdu"].raw_put(pdu_id(3), Json(undecodable));
	index.insert(&relation_key(0x02, 4), u64::MAX.to_be_bytes());
	index.insert(&relation_key(0x02, 2), 2_u64.to_be_bytes());
	index.insert(&relation_key(0x01, 7), 7_u64.to_be_bytes());
	index.insert(b"short", []);

	let summary = walk(services, "relation-index").await?;

	assert_eq!(summary.counts.get("missing-row"), Some(&1));
	assert_eq!(summary.counts.get("mismatched-child"), Some(&1));
	assert_eq!(summary.counts.get("stale-row"), Some(&1));
	assert_eq!(summary.counts.get("absent-child"), Some(&1));
	assert_eq!(summary.counts.get("unparsable-key"), Some(&1));
	assert_eq!(summary.counts.get("unparsable-event"), Some(&1));
	assert_eq!(summary.counts.get("unreadable-parent"), Some(&1));

	services.db["global"].remove("rebuild_relatesto_typed");
	migrate(services, false).await?;

	let summary = walk(services, "relation-index").await?;

	assert_eq!(summary.counts.get("unparsable-event"), Some(&1));
	assert_eq!(summary.counts.get("unreadable-parent"), Some(&1));
	assert_eq!(summary.findings(), 2);
	assert!(present(index, &relation_key(0x01, 2)).await);
	assert!(!present(index, &relation_key(0x02, 2)).await);

	Ok(())
}

#[tokio::test]
async fn relation_index_reports_a_malformed_short_id() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let index = &services.db["relatesto_typed"];
	let room = room_id!("!room:localhost");
	let alice = user_id!("@alice:localhost");
	let (parent, edit) = (event_id!("$parent:localhost"), event_id!("$edit:localhost"));
	let replace = json!({ "m.relates_to": { "rel_type": "m.replace", "event_id": parent } });
	let message = pdu_json(parent, room, alice, "m.room.message", &json!({}));
	let overlong = overlong(2);

	fresh(services).await?;
	store(services, 1, 1, parent, &message);
	store(services, 1, 2, edit, &pdu_json(edit, room, alice, "m.room.message", &replace));
	index.insert(&relation_key(0x01, 2), 2_u64.to_be_bytes());

	for short in [&b"bad"[..], &overlong] {
		services.db["eventid_shorteventid"].insert(edit.as_bytes(), short);

		let summary = walk(services, "relation-index").await?;

		assert_eq!(summary.counts.get("malformed-short-id"), Some(&1), "{short:?}");
		assert_eq!(summary.findings(), 1, "{short:?}");
	}

	Ok(())
}

/// Builds a `relatesto_typed` key under the parent at count 1 of room 1.
///
/// Every child in these tests carries timestamp 1, as `pdu_json` gives it.
fn relation_key(tag: u8, child: u64) -> Vec<u8> {
	let one = 1_u64.to_be_bytes();

	[&one[..], &one, &[tag], &one, &child.to_be_bytes()].concat()
}

fn store(services: &Services, shortroomid: ShortRoomId, count: u64, id: &EventId, pdu: &Value) {
	let pdu_id: RawPduId = PduId {
		shortroomid,
		count: PduCount::Normal(count),
	}
	.into();

	services.db["eventid_pduid"].insert(id.as_bytes(), pdu_id.as_bytes());
	services.db["pduid_pdu"].raw_put(pdu_id, Json(pdu));
}

#[tokio::test]
async fn thread_summaries_repair_clears_the_finding() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let roots = &services.db["threadid_userids"];
	let thread = json!({ "m.relations": { "m.thread": { "count": 3 } } });
	let root = json!({ "room_id": "!thread:localhost", "unsigned": thread });

	fresh(services).await?;
	services.db["pduid_pdu"].raw_put(pdu_id(1), Json(root));
	services.db["pduid_pdu"].insert(&pdu_id(5), b"not json");
	roots.insert(&pdu_id(1), "@alice:localhost");
	roots.insert(&pdu_id(5), "@alice:localhost");

	let summary = walk(services, "thread-summaries").await?;

	assert_eq!(summary.counts.get("stale-summary"), Some(&1));
	assert_eq!(summary.counts.get("unreadable-root"), Some(&1));

	services.db["global"].remove("rebuild_thread_summaries");
	migrate(services, false).await?;

	// The summary rebuild decodes keys without checking them, so this one comes after it.
	roots.insert(b"short", "@alice:localhost");

	let summary = walk(services, "thread-summaries").await?;

	assert_eq!(summary.counts.get("unreadable-root"), Some(&1));
	assert_eq!(summary.counts.get("unparsable-key"), Some(&1));
	assert_eq!(summary.findings(), 2);

	Ok(())
}

#[tokio::test]
async fn thread_summaries_report_a_short_relation_key() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let thread = json!({ "m.relations": { "m.thread": { "count": 1 } } });
	let root = json!({ "room_id": "!thread:localhost", "unsigned": thread });
	let short = [&PduCount::Normal(1).to_be_bytes()[..], &[0]].concat();

	fresh(services).await?;
	services.db["pduid_pdu"].raw_put(pdu_id(1), Json(root));
	services.db["threadid_userids"].insert(&pdu_id(1), "@alice:localhost");
	services.db["tofrom_relation"].insert(&short, []);

	let summary = walk(services, "thread-summaries").await?;

	assert_eq!(summary.counts.get("malformed-relation"), Some(&1));
	assert_eq!(summary.findings(), 1);

	services.db["global"].remove("rebuild_thread_summaries");
	migrate(services, false).await?;

	let summary = walk(services, "thread-summaries").await?;
	let root = services
		.timeline
		.get_pdu_json_from_id(&pdu_id(1))
		.await?;

	assert_eq!(summary.counts.get("malformed-relation"), Some(&1));
	assert_eq!(summary.findings(), 1);
	assert_eq!(serde_json::to_value(root)?["unsigned"]["m.relations"]["m.thread"]["count"], 1);
	assert_eq!(services.threads.rebuild_thread_summaries().await, (0, 1));

	Ok(())
}

#[tokio::test]
async fn thread_activity_repair_clears_the_finding() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let activity = &services.db["threadactivityid_rootid"];
	let pointers = &services.db["threadrootid_latestcount"];
	let pointer = |count: u64| PduCount::Normal(count).to_be_bytes();
	let thread = json!({ "m.relations": { "m.thread": { "count": 1 } } });
	let root = json!({ "room_id": "!thread:localhost", "unsigned": thread });

	fresh(services).await?;
	services.db["pduid_pdu"].raw_put(pdu_id(1), Json(root));
	services.db["threadid_userids"].insert(&pdu_id(1), "@alice:localhost");
	pointers.insert(&pdu_id(1), pointer(5));
	pointers.insert(&pdu_id(3), pointer(6));
	activity.insert(&pdu_id(6), pdu_id(9));
	pointers.insert(b"short", pointer(1));

	let summary = walk(services, "thread-activity").await?;

	assert_eq!(summary.counts.get("dangling-pointer"), Some(&1));
	assert_eq!(summary.counts.get("misdirected-pointer"), Some(&1));
	assert_eq!(summary.counts.get("unparsable-pointer"), Some(&1));

	services.db["global"].remove("rebuild_live_thread_activity");
	migrate(services, false).await?;

	let summary = walk(services, "thread-activity").await?;

	assert_eq!(summary.findings(), 0);
	assert_eq!(&*pointers.get(&pdu_id(1)).await?, pointer(1).as_slice());
	assert_eq!(&*activity.get(&pdu_id(1)).await?, pdu_id(1).as_bytes());

	Ok(())
}

#[tokio::test]
async fn joined_members_repair_clears_the_finding() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!room:localhost");
	let (alice, bob, carol) = (
		user_id!("@alice:localhost"),
		user_id!("@bob:localhost"),
		user_id!("@carol:localhost"),
	);

	let create = json!({ "creator": alice, "room_version": "10" });
	let state = [
		(1, event_id!("$create:localhost"), "m.room.create", "", create),
		(
			2,
			event_id!("$alice:localhost"),
			"m.room.member",
			alice.as_str(),
			json!({ "membership": "join" }),
		),
		(
			3,
			event_id!("$bob:localhost"),
			"m.room.member",
			bob.as_str(),
			json!({ "membership": "leave" }),
		),
	];

	fresh(services).await?;
	seed_room_state(services, room, alice, &state).await?;

	for (count, user_id) in [(4, alice), (5, bob), (6, carol)] {
		services
			.state_cache
			.mark_as_joined(user_id, room, PduCount::Normal(count));
	}

	let summary = walk(services, "joined-members").await?;

	assert_eq!(summary.counts.get("contradicted-join"), Some(&1));
	assert_eq!(summary.counts.get("unbacked-join"), Some(&1));
	assert_eq!(summary.findings(), 2);

	services.db["global"].remove("retroactively_fix_bad_data_from_roomuserid_joined");
	migrate(services, false).await?;

	let summary = walk(services, "joined-members").await?;

	assert_eq!(summary.counts.get("unbacked-join"), Some(&1));
	assert_eq!(summary.findings(), 1);
	assert!(services.state_cache.is_joined(alice, room).await);
	assert!(services.state_cache.is_left(bob, room).await);

	Ok(())
}

/// Stores a room's state events and makes them its current state.
///
/// Each event lands in the timeline at its count before the state is compressed
/// and saved.
async fn seed_room_state(
	services: &Services,
	room_id: &RoomId,
	sender: &UserId,
	events: &[StateEvent<'_>],
) -> Result {
	let shortroomid = services
		.short
		.get_or_create_shortroomid(room_id)
		.await;

	for (count, id, kind, state_key, content) in events {
		let pdu = with_state_key(pdu_json(id, room_id, sender, kind, content), state_key);

		store(services, shortroomid, *count, id, &pdu);
	}

	let state = events
		.iter()
		.stream()
		.then(async |&(_, id, kind, state_key, _)| {
			compress_state(services, &StateEventType::from(kind), state_key, id).await
		})
		.collect()
		.await;

	save_room_state(services, room_id, Arc::new(state)).await?;

	Ok(())
}
