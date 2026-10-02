use std::{collections::BTreeSet, iter::once};

use serde_json::{Value, json};
use tuwunel_core::{Result, config::Figment, utils::BoolExt};
use tuwunel_database::Database;

use super::{
	super::identity::{census as census_identities, repair as repair_identities},
	Census, CompressedState, Mapping, Parents, Projection, States, apply, decode, depth, index,
	materialize, repair,
};
use crate::{
	Services,
	rooms::state_compressor::ShortStateInfo,
	test_utils::{fixture, pdu_id},
};

#[test]
fn strict_framing_and_actual_parentless_semantics() {
	for invalid in [vec![0; 7], words(&[0, 0]), words(&[0, 1]), words(&[0, 0, 0, 2]), vec![0; 9]]
	{
		assert!(decode(&invalid).is_none());
	}

	let parsed = decode(&diff(0, &[(12, 101)], &[(12, 101)])).expect("framed parentless row");
	let result = apply(CompressedState::new(), parsed);

	assert_eq!(result.len(), 1, "parentless rows ignore removes under the existing codec");
	let parents: Parents =
		[(9, None), (2, Some(9)), (3, Some(2)), (7, Some(8)), (8, Some(7)), (5, Some(6))].into();

	assert_eq!(depth(&parents, 3), Some(3), "dependency order is not numeric order");

	assert_eq!(depth(&parents, 7), None, "cycles do not supply a root");

	assert_eq!(depth(&parents, 5), None, "missing ancestors are not empty states");
	let empty = || Mapping { aliases: [].into(), refused: [].into() };
	let projection = Projection { statekeys: empty(), events: empty() };
	let census = Census {
		parents: Parents::new(),
		seeds: BTreeSet::new(),
		unknown: false,
		held: Vec::new(),
		orphans: [].into(),
		complete: true,
		projection,
	};

	let census = index(census, None, Some(9), false);

	assert!(!census.unknown, "readable parent survives a malformed row key");

	assert_eq!(census.held, [9]);
}

#[tokio::test]
#[tracing::instrument(level = "trace", skip_all)]
async fn snapshots_preserve_original_logical_state_and_all_descendants() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let db = &services.db;
	let cache = &services.state_compressor.stateinfo_cache;
	let statehashes = &db["statehash_shortstatehash"];

	identities(db);
	branch(db);

	for (digest, id) in [
		(b"digest1".as_slice(), 1000_u64),
		(b"digest2", 1000),
		(b"digest3", 1400),
		(b"neighbor", 2000),
	] {
		statehashes.insert(digest, id.to_be_bytes());
	}

	statehashes.insert(b"tailed", [words(&[1000]), vec![99]].concat());

	db["roomid_spacehierarchy"].insert(b"!room:example.org", b"cached summary");

	cache
		.lock()
		.expect("test cache lock")
		.insert(1200, vec![ShortStateInfo::default()]);

	let repaired = repair_identities(services).await?;
	let states = repair(services, &repaired).await?;

	assert!(states.unfinished.is_empty());
	assert!(!states.uncertain);
	assert_absent(
		db,
		"roomid_spacehierarchy",
		b"!room:example.org",
		"cached summary invalidated",
	)
	.await;

	assert_eq!(states.rewritten, 5);

	assert_eq!(states.original_bytes, 152);

	assert_eq!(states.snapshot_bytes, 184);

	for id in [1000, 1100, 1200] {
		assert_snapshot(db, id, &[(12, 101), (21, 102)]).await?;
	}

	assert_snapshot(db, 1300, &[(21, 102)]).await?;

	assert_snapshot(db, 1400, &[(21, 102), (22, 103)]).await?;

	assert_snapshot(db, 2000, &[(23, 104)]).await?;

	for digest in [b"digest1".as_slice(), b"digest2", b"digest3", b"tailed"] {
		assert_absent(db, "statehash_shortstatehash", digest, "all row digests deleted").await;
	}

	let stored = statehashes.get(b"neighbor").await?;

	assert_eq!(stored.as_ref(), words(&[2000]));

	assert!(cache.lock().expect("test cache lock").is_empty());

	let repeated = rerun(services).await?;

	assert_eq!(repeated.rewritten, 0, "completed snapshots rediscover no residue");
	assert!(repeated.unfinished.is_empty());

	Ok(())
}

#[tokio::test]
#[tracing::instrument(level = "trace", skip_all)]
async fn uncertain_branches_retain_ancestors_while_proven_children_detach() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let db = &services.db;

	identities(db);
	let rows = [
		(900_u64, diff(0, &[(11, 101), (12, 103)], &[])),
		(800, diff(900, &[], &[(12, 103)])),
		(700, diff(0, &[(11, 101)], &[])),
		(600, words(&[900, 11])),
		(500, diff(400, &[(11, 101)], &[])),
		(400, diff(500, &[(11, 101)], &[])),
		(300, diff(999, &[(11, 101)], &[])),
		(200, diff(0, &[(11, 101)], &[(11, 101)])),
		(100, diff(200, &[], &[])),
	];

	for (id, row) in &rows {
		insert_diff(db, *id, row);
	}

	let states = repair(services, &repair_identities(services).await?).await?;

	assert_eq!(states.rewritten, 2, "safe child and disjoint row repair independently");

	assert_snapshot(db, 800, &[(12, 101)]).await?;

	assert_snapshot(db, 700, &[(12, 101)]).await?;

	for (id, original) in rows
		.iter()
		.filter(|(id, _)| ![800, 700].contains(id))
	{
		assert_original(db, *id, original).await?;

		assert!(states.unfinished.contains(id));
	}

	assert!(
		materialize(services, 100).await?.is_none(),
		"intersection ancestor withholds descendants"
	);

	Ok(())
}

#[tokio::test]
#[tracing::instrument(level = "trace", skip_all)]
async fn malformed_parent_and_prefix_claims_withhold_only_required_work() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let db = &services.db;
	let statediff = &db["shortstatehash_statediff"];
	let statekeys = &db["statekey_shortstatekey"];

	identities(db);

	insert_diff(db, 1000, diff(0, &[(11, 101)], &[]));

	insert_diff(db, 2000, diff(0, &[(11, 101)], &[]));

	statediff.insert(b"malformed-key", words(&[1000, 23, 104]));
	let states = rerun(services).await?;

	assert_eq!(states.rewritten, 1);
	assert!(states.unfinished.contains(&1000));

	assert_snapshot(db, 2000, &[(12, 101)]).await?;

	statediff.remove(b"malformed-key");

	statekeys.insert(b"m.room.extra\xff", [words(&[12]), vec![88]].concat());
	let states = rerun(services).await?;

	assert_eq!(states.rewritten, 0, "release-codec prefix claimant is not ignored");

	statekeys.remove(b"m.room.extra\xff");

	statediff.insert(b"unknown", vec![1; 7]);
	let states = rerun(services).await?;

	assert_eq!(states.rewritten, 0);
	assert!(states.uncertain, "unknown parent can hide any descendant");

	Ok(())
}

#[tokio::test]
#[tracing::instrument(level = "trace", skip_all)]
async fn recoverable_key_orphans_keep_durable_identity_and_ambiguous_orphans_stay() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let db = &services.db;

	identities(db);
	accepted_pdu(db, 101, "m.room.name");
	accepted_pdu(db, 102, "m.room.topic");
	accepted_pdu(db, 104, "m.room.name");
	let mismatching = db["pduid_pdu"]
		.get(pdu_id(101).as_bytes())
		.await?;

	db["pduid_pdu"].insert(pdu_id(104).as_bytes(), mismatching.as_ref());

	let rows = [
		(1000_u64, diff(0, &[(30, 101)], &[])),
		(1100, diff(1000, &[], &[(30, 101)])),
		(2000, diff(0, &[(31, 101), (31, 102)], &[])),
		(3000, diff(0, &[(32, 103)], &[])),
		(4000, diff(0, &[(12, 999)], &[])),
		(4500, diff(0, &[(34, 104)], &[])),
	];

	for (id, row) in &rows {
		insert_diff(db, *id, row);
	}

	let states = repair(services, &repair_identities(services).await?).await?;

	assert_eq!(states.rewritten, 2);
	let stored = db["shortstatekey_statekey"]
		.get(&30_u64.to_be_bytes())
		.await?;

	assert_eq!(stored.as_ref(), b"m.room.name\xff");

	assert_snapshot(db, 1000, &[(12, 101)]).await?;

	assert_snapshot(db, 1100, &[]).await?;

	for (id, original) in rows.iter().filter(|(id, _)| *id >= 2000) {
		assert_original(db, *id, original).await?;

		assert!(states.unfinished.contains(id));
	}

	for short in [31_u64, 32, 34] {
		assert_absent(
			db,
			"shortstatekey_statekey",
			&short.to_be_bytes(),
			"unproven orphan stays absent",
		)
		.await;
	}

	assert_absent(
		db,
		"shorteventid_eventid",
		&999_u64.to_be_bytes(),
		"no event identity allocated",
	)
	.await;

	let repeated = rerun(services).await?;

	assert_eq!(repeated.rewritten, 0);

	assert_eq!(repeated.unfinished, states.unfinished);

	db["shortstatekey_statekey"].remove(&30_u64.to_be_bytes());

	db["statekey_shortstatekey"].insert(b"m.room.extra\xff", [words(&[12]), vec![88]].concat());
	let orphan = words(&[0, 33, 101]);

	db["shortstatehash_statediff"].insert(&5000_u64.to_be_bytes(), &orphan);
	let held = repair(services, &census_identities(services).await?).await?;

	assert_eq!(held.rewritten, 0, "new alias admission checks its winner's hidden claimant");
	assert!(held.unfinished.contains(&5000));
	assert_original(db, 5000, &orphan).await?;

	let stored = db["shortstatekey_statekey"]
		.get(&33_u64.to_be_bytes())
		.await?;

	assert_eq!(
		stored.as_ref(),
		b"m.room.name\xff",
		"restored identity remains durable when substitution is refused"
	);

	Ok(())
}

fn words(values: &[u64]) -> Vec<u8> {
	values
		.iter()
		.copied()
		.flat_map(u64::to_be_bytes)
		.collect()
}

fn identities(db: &Database) {
	statekey_identities(db, &[
		(12, b"m.room.name\xff"),
		(21, b"m.room.topic\xff"),
		(22, b"m.room.member\xff@b:example.org"),
		(23, b"m.room.member\xff@c:example.org"),
	]);

	// 11 is a reverse-only alias of 12, so snapshots key its entries by 12.
	db["shortstatekey_statekey"].insert(&11_u64.to_be_bytes(), b"m.room.name\xff");

	for short in 101_u64..=104 {
		event_identity(db, short);
	}
}

fn statekey_identities(db: &Database, statekeys: &[(u64, &[u8])]) {
	for &(short, key) in statekeys {
		db["statekey_shortstatekey"].insert(key, short.to_be_bytes());

		db["shortstatekey_statekey"].insert(&short.to_be_bytes(), key);
	}
}

fn event_identity(db: &Database, short: u64) {
	let event_id = event_id(short);

	db["eventid_shorteventid"].insert(&event_id, short.to_be_bytes());

	db["shorteventid_eventid"].insert(&short.to_be_bytes(), event_id);
}

fn branch(db: &Database) {
	for (id, parent, added, removed) in [
		(1000_u64, 0_u64, vec![(11, 101), (21, 102)], vec![]),
		(1100, 1000, vec![(12, 101)], vec![]),
		(1200, 1100, vec![], vec![(12, 101)]),
		(1300, 1000, vec![], vec![(11, 101)]),
		(1400, 1300, vec![(22, 103)], vec![]),
		(2000, 0, vec![(23, 104)], vec![]),
	] {
		insert_diff(db, id, diff(parent, &added, &removed));
	}
}

fn insert_diff(db: &Database, id: u64, row: impl AsRef<[u8]>) {
	db["shortstatehash_statediff"].insert(&id.to_be_bytes(), row);
}

#[tracing::instrument(level = "trace", skip_all)]
async fn assert_absent(db: &Database, column: &str, key: &[u8], message: &str) {
	let error = db[column].get(key).await.expect_err(message);

	assert!(error.is_missing(), "{message}: {error}");
}

#[tracing::instrument(level = "trace", skip_all)]
async fn assert_snapshot(db: &Database, id: u64, expected: &[(u64, u64)]) -> Result {
	assert_original(db, id, &diff(0, expected, &[])).await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn assert_original(db: &Database, id: u64, original: &[u8]) -> Result {
	let stored = db["shortstatehash_statediff"]
		.get(&id.to_be_bytes())
		.await?;

	assert_eq!(stored.as_ref(), original);

	Ok(())
}

fn diff(parent: u64, added: &[(u64, u64)], removed: &[(u64, u64)]) -> Vec<u8> {
	let separator = removed.is_empty().is_false().then_some(0);

	once(parent)
		.chain(entries(added))
		.chain(separator)
		.chain(entries(removed))
		.flat_map(u64::to_be_bytes)
		.collect()
}

fn entries(pairs: &[(u64, u64)]) -> impl Iterator<Item = u64> + '_ {
	pairs
		.iter()
		.flat_map(|&(key, event)| [key, event])
}

#[tracing::instrument(level = "trace", skip_all)]
async fn rerun(services: &Services) -> Result<States> {
	repair(services, &census_identities(services).await?).await
}

fn accepted_pdu(db: &Database, short: u64, kind: &str) {
	let room_id = "!room:example.org";
	let pdu = json!({
		"event_id": event_id(short),
		"room_id": room_id,
		"sender": "@user:example.org",
		"type": kind,
		"state_key": "",
		"content": {},
		"prev_events": [],
		"auth_events": [],
		"depth": 1,
		"origin_server_ts": 1,
		"hashes": { "sha256": "test" },
	});

	db["roomid_shortroomid"].insert(room_id, 1_u64.to_be_bytes());

	insert_pdu(db, short, &pdu);
}

fn insert_pdu(db: &Database, short: u64, pdu: &Value) {
	let id = pdu_id(short);

	db["eventid_pduid"].insert(&event_id(short), id.as_bytes());

	db["pduid_pdu"].insert(id.as_bytes(), serde_json::to_vec(pdu).expect("fixture JSON"));
}

fn event_id(short: u64) -> String { format!("$event{short}:example.org") }
