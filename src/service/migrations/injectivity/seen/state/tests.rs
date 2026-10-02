use std::{
	collections::BTreeSet,
	io::{Error as IoError, ErrorKind as IoErrorKind},
	iter::once,
};

use serde_json::{Value, json};
use tuwunel_core::{Error, Result, config::Figment, err, smallvec::SmallVec, utils::BoolExt};
use tuwunel_database::Database;

use super::{
	super::identity::{census as census_identities, repair as repair_identities},
	Census, CompressedState, Mapping, Parents, Projection, States, apply, census, decode, depth,
	digests,
	history::{allowed, read_error, references as history_references},
	index, mapping, materialize, publish, repair, target,
};
use crate::{
	Services,
	rooms::{state_compressor::ShortStateInfo, state_res::AuthCheckOutcome},
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
		exposed: BTreeSet::new(),
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

#[test]
fn authorization_evidence_errors_preserve_operational_failures() -> Result {
	assert!(allowed(Ok(AuthCheckOutcome::Allow))?);
	assert!(!allowed(Ok(AuthCheckOutcome::Deny(err!("rejected event"))))?);

	for invalid in [
		Error::Database("invalid authorization dependency".into()),
		Error::SerdeDe("invalid event record".into()),
		err!(Request(BadJson("invalid content"))),
		err!(Request(NotFound("missing auth input"))),
	] {
		assert!(!allowed(Err(invalid))?);
	}

	let malformed: Result<Value> = serde_json::from_slice(b"{").map_err(Into::into);

	assert!(!allowed(malformed.map(|_| AuthCheckOutcome::Allow))?);

	let raw = IoError::new(IoErrorKind::PermissionDenied, "storage read failed");
	let operational =
		allowed(Err(read_error(raw.into()))).expect_err("storage failure remains operational");

	let Error::Io(raw) = operational else {
		panic!("storage error lost its variant");
	};

	assert_eq!(raw.kind(), IoErrorKind::PermissionDenied);

	let absent: Error = IoError::new(IoErrorKind::NotFound, "engine read failed").into();

	assert!(absent.is_not_found(), "unprotected engine failure resembles missing evidence");
	let protected = read_error(absent);

	assert!(!protected.is_not_found(), "auth must not reinterpret an engine failure");
	let operational = allowed(Err(protected)).expect_err("engine failure remains operational");

	let Error::Io(protected) = operational else {
		panic!("engine error lost its protected origin");
	};

	assert_eq!(protected.kind(), IoErrorKind::Other);
	let source = protected
		.get_ref()
		.and_then(|source| source.downcast_ref());

	assert!(matches!(source, Some(Error::Io(source)) if source.kind() == IoErrorKind::NotFound));

	let pool = allowed(Err(read_error(err!("recv failed"))))
		.expect_err("pool failure cannot become invalid authorization content");

	let Error::Io(pool) = pool else {
		panic!("pool error lost its protected origin");
	};

	assert_eq!(pool.kind(), IoErrorKind::Other);
	let origin = pool
		.get_ref()
		.and_then(|source| source.downcast_ref());

	assert!(matches!(origin, Some(Error::Err(..))));

	Ok(())
}

#[tokio::test]
#[tracing::instrument(level = "trace", skip_all)]
async fn historical_intersections_require_accepted_agreeing_anchors_and_preserve_intact_writers()
-> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let db = &services.db;

	historical_fixture(db);

	db["global"].insert(b"fix_short_injectivity", b"");
	let clean = rerun(services).await?;

	assert_eq!(clean.rewritten, 0, "old exposure without intersections is harmless");

	assert!(clean.unfinished.is_empty());

	let damaged = diff(1000, &[(3, 102)], &[(3, 102)]);
	let descendant = diff(2000, &[(4, 103)], &[]);
	let reset = || {
		insert_diff(db, 2000, &damaged);

		insert_diff(db, 3000, &descendant);
	};

	reset();

	db["shorteventid_shortstatehash"].remove(&102_u64.to_be_bytes());
	let unknown = rerun(services).await?;

	assert_eq!(unknown.rewritten, 0);

	assert_eq!(unknown.unfinished, BTreeSet::from([2000, 3000]));

	assert_original(db, 2000, &damaged).await?;

	assert_original(db, 3000, &descendant).await?;

	db["shorteventid_shortstatehash"].insert(&102_u64.to_be_bytes(), 1000_u64.to_be_bytes());

	let member_id = pdu_id(101);
	let membership = db["pduid_pdu"]
		.get(member_id.as_bytes())
		.await?
		.to_vec();

	let invalid_member = changed(serde_json::from_slice(&membership)?, "content", json!({}));

	db["pduid_pdu"].insert(member_id.as_bytes(), serde_json::to_vec(&invalid_member)?);

	db["shortstatekey_statekey"].insert(&14_u64.to_be_bytes(), b"m.room.topic\xff");

	insert_diff(db, 4000, diff(0, &[(14, 103)], &[]));
	let severed = rerun(services).await?;

	assert_eq!(severed.rewritten, 1, "unrelated state progresses beside invalid auth content");

	assert_eq!(severed.unfinished, BTreeSet::from([2000, 3000]));

	assert_original(db, 2000, &damaged).await?;

	assert_original(db, 3000, &descendant).await?;

	assert_snapshot(db, 4000, &[(4, 103)]).await?;

	db["shortstatehash_statediff"].remove(&4000_u64.to_be_bytes());

	db["shortstatekey_statekey"].remove(&14_u64.to_be_bytes());

	db["pduid_pdu"].remove(member_id.as_bytes());
	let unavailable = rerun(services).await?;

	assert_eq!(unavailable.rewritten, 0, "missing accepted auth state is not an absent tuple");

	db["pduid_pdu"].insert(member_id.as_bytes(), &membership);

	let topic_id = pdu_id(103);
	let topic = db["pduid_pdu"]
		.get(topic_id.as_bytes())
		.await?
		.to_vec();

	let pdu: Value = serde_json::from_slice(&topic)?;
	let missing = changed(
		pdu,
		"auth_events",
		json!(["$event100:example.org", "$event101:example.org", "$missing:example.org"]),
	);

	db["pduid_pdu"].insert(topic_id.as_bytes(), serde_json::to_vec(&missing)?);

	let unavailable = rerun(services).await?;

	assert_eq!(
		unavailable.rewritten, 0,
		"late missing auth input cannot reuse an earlier complete graph"
	);

	db["pduid_pdu"].insert(topic_id.as_bytes(), &topic);

	insert_diff(db, 1100, diff(0, &[(1, 100), (2, 101)], &[]));

	db["shorteventid_shortstatehash"].insert(&105_u64.to_be_bytes(), 1100_u64.to_be_bytes());

	db["shorteventid_shortstatehash"].insert(&106_u64.to_be_bytes(), 2000_u64.to_be_bytes());
	let conflicted = rerun(services).await?;

	assert!(conflicted.unfinished.contains(&2000), "valid historical anchors must agree");

	assert_original(db, 2000, &damaged).await?;

	db["shorteventid_shortstatehash"].remove(&106_u64.to_be_bytes());

	reset();

	for column in ["roomid_shortstatehash", "eventid_resolvedstate"] {
		db[column].insert(b"unattributed", 3000_u64.to_be_bytes());
		let held = rerun(services).await?;

		assert_eq!(held.rewritten, 0, "unattributed live referrer protects its ancestors");

		db[column].remove(b"unattributed");
	}

	insert_diff(db, 3000, diff(2000, &[(3, 105), (4, 103)], &[]));
	let contradictory = rerun(services).await?;

	assert_eq!(
		contradictory.rewritten, 0,
		"intact descendant assignment at damaged key cannot be replaced"
	);

	assert_original(db, 2000, &damaged).await?;

	reset();

	for (digest, id) in [(b"old-c".as_slice(), 2000_u64), (b"old-d", 3000)] {
		db["statehash_shortstatehash"].insert(digest, id.to_be_bytes());
	}

	let repaired = rerun(services).await?;

	assert_eq!(repaired.rewritten, 2);

	assert!(repaired.unfinished.is_empty());

	assert_snapshot(db, 2000, &[(1, 100), (2, 101), (3, 102)]).await?;

	assert_snapshot(db, 3000, &[(1, 100), (2, 101), (3, 102), (4, 103)]).await?;

	assert_snapshot(db, 1000, &[(1, 100), (2, 101), (3, 105)]).await?;

	for digest in [b"old-c".as_slice(), b"old-d"] {
		assert_absent(db, "statehash_shortstatehash", digest, "rewritten digest removed").await;
	}

	reset();
	let identity_census = census_identities(services).await?;
	let statekeys = mapping(services, &identity_census.statekeys).await?;
	let events = mapping(services, &identity_census.events).await?;
	let projection = Projection { statekeys, events };

	let scanned = census(services, projection).await?;
	let affected = BTreeSet::from([2000, 3000]);
	let digests = digests(services, &affected).await?;
	let references = history_references(services, affected).await?;
	let child = target(services, &scanned.projection, &references, 3000)
		.await?
		.expect("independently proved child");

	let child_digests = digests
		.get(&3000)
		.map(SmallVec::as_slice)
		.unwrap_or_default();

	publish(services, &scanned.projection, &references, 3000, &child, child_digests).await?;

	let fresh = census(services, scanned.projection).await?;

	assert!(
		fresh.exposed.contains(&2000),
		"interruption after child leaves the ancestor's seed durable"
	);

	assert!(!fresh.exposed.contains(&3000), "verified child is detached");

	assert_original(db, 2000, &damaged).await?;

	let resumed = rerun(services).await?;

	assert_eq!(resumed.rewritten, 1);

	assert_snapshot(db, 2000, &[(1, 100), (2, 101), (3, 102)]).await?;

	Ok(())
}

#[tokio::test]
#[tracing::instrument(level = "trace", skip_all)]
async fn historical_roots_serve_intersections_over_disagreeing_reconstructions() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let db = &services.db;

	historical_fixture(db);

	// A parentless row serves its added entries and ignores its removes.
	let served = diff(0, &[(1, 100), (2, 101), (3, 105)], &[(3, 105)]);

	insert_diff(db, 2000, &served);
	let withheld = rerun(services).await?;

	assert_eq!(
		withheld.rewritten, 0,
		"a served root entry outranks a disagreeing reconstruction"
	);

	assert_eq!(withheld.unfinished, BTreeSet::from([2000]));

	assert_original(db, 2000, &served).await?;

	let agreeing = diff(0, &[(1, 100), (2, 101), (3, 102)], &[(3, 102)]);

	insert_diff(db, 2000, &agreeing);
	let published = rerun(services).await?;

	assert_eq!(published.rewritten, 1, "the same anchors publish a root that agrees");

	assert!(published.unfinished.is_empty());

	assert_snapshot(db, 2000, &[(1, 100), (2, 101), (3, 102)]).await?;

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

fn historical_fixture(db: &Database) {
	statekey_identities(db, &[
		(1, b"m.room.create\xff"),
		(2, b"m.room.member\xff@user:example.org"),
		(3, b"m.room.name\xff"),
		(4, b"m.room.topic\xff"),
	]);

	let topic_auth = vec![100, 101];
	let room_id = "!room:example.org";

	for (short, kind, key, content, previous, auth) in [
		(
			100_u64,
			"m.room.create",
			Some(""),
			json!({"creator":"@user:example.org","room_version":"6"}),
			vec![],
			vec![],
		),
		(
			101,
			"m.room.member",
			Some("@user:example.org"),
			json!({"membership":"join"}),
			vec![100],
			vec![100],
		),
		(102, "m.room.name", Some(""), json!({"name":"new"}), vec![105], vec![100, 101]),
		(103, "m.room.topic", Some(""), json!({"topic":"later"}), vec![102], topic_auth),
		(
			104,
			"m.room.message",
			None,
			json!({"msgtype":"m.text","body":"after"}),
			vec![103],
			vec![100, 101],
		),
		(105, "m.room.name", Some(""), json!({"name":"old"}), vec![101], vec![100, 101]),
		(
			106,
			"m.room.message",
			None,
			json!({"msgtype":"m.text","body":"fork"}),
			vec![105],
			vec![100, 101],
		),
	] {
		event_identity(db, short);
		let pdu = json!({
			"event_id": event_id(short),
			"room_id": room_id,
			"sender": "@user:example.org",
			"type": kind,
			"state_key": key,
			"content": content,
			"prev_events": event_ids(&previous),
			"auth_events": event_ids(&auth),
			"depth": short,
			"origin_server_ts": short,
			"hashes": { "sha256": "test" },
		});

		insert_pdu(db, short, &pdu);
	}

	db["roomid_shortroomid"].insert(room_id, 1_u64.to_be_bytes());

	insert_diff(db, 1000, diff(0, &[(1, 100), (2, 101), (3, 105)], &[]));

	for (event, state) in [(102_u64, 1000_u64), (103, 2000), (104, 3000)] {
		db["shorteventid_shortstatehash"].insert(&event.to_be_bytes(), state.to_be_bytes());
	}
}

fn event_ids(shorts: &[u64]) -> Vec<String> { shorts.iter().copied().map(event_id).collect() }

fn changed(mut pdu: Value, key: &str, value: Value) -> Value {
	pdu[key] = value;
	pdu
}
