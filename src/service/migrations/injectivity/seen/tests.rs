use std::{cell::Cell, fmt::Debug, iter::repeat_with, ops::RangeInclusive, sync::Arc};

use futures::future::ready;
use ruma::{RoomId, events::StateEventType, room_id, user_id};
use tuwunel_core::{
	Err, Result, Server,
	config::{Config, Figment, Sources},
	log::{LogLevelReloadHandles, Logging},
	utils::{TryReadyExt, result::NotFound},
};
use tuwunel_database::{Database, Txn, TxnError};

use super::{
	Boundary, IDENTITY_LIMIT, MARKER, Outcome, Reason, SAMPLE_LIMIT, Sample, Shape, Status,
	Uncertain,
	identity::{Kind, census as identity_census, consistent, repair as repair_identities},
	references::{References, entries},
	run, stamp, stands, verify,
};
use crate::{
	Services,
	migrations::{
		injectivity::{CACHE_CLEARED, SUPERSEDED, fix},
		migrations,
	},
	test_utils::fixture,
};

const EVENTS: (&str, &str) = ("eventid_shorteventid", "shorteventid_eventid");
const STATEKEYS: (&str, &str) = ("statekey_shortstatekey", "shortstatekey_statekey");

// Spans several `CACHE_BATCH` deletion batches.
const CACHED: RangeInclusive<u64> = 1000..=1199;

const SPACE: &[u8] = b"!space:example.org";

#[tokio::test]
async fn runner_contains_errors_and_honors_only_its_marker() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let db = &fixture.services.db;
	let server = &fixture.services.server;
	let global = &db["global"];
	let calls = Cell::new(0);
	let repair = || {
		calls.set(calls.get() + 1);
		ready(Ok(Outcome::clean()))
	};

	assert_eq!(run(db, repair).await, Some(Outcome::clean()));
	assert_eq!(calls.get(), 1);
	global.remove(MARKER);
	calls.set(0);

	for old in [b"".as_slice(), b"declined", b"unknown"] {
		global.insert("fix_short_injectivity", old);
		assert_eq!(run(db, repair).await, Some(Outcome::clean()));
		global.remove(MARKER);
	}

	assert_eq!(calls.get(), 3);
	assert_absent(db, "global", MARKER, "new record removed between cases").await;
	global.insert(MARKER, b"unknown");
	assert_eq!(run(db, repair).await, Some(Outcome::clean()));
	assert_eq!(calls.get(), 4);
	global.insert(MARKER, Outcome::clean().encode()?);
	assert!(run(db, repair).await.is_none());
	assert_eq!(calls.get(), 4);
	global.remove(MARKER);
	let unfinished = Outcome {
		status: Status::Unfinished,
		counts: [1; 15],
		..Outcome::clean()
	};

	assert_eq!(run(db, || ready(Ok(unfinished.clone()))).await, Some(unfinished.clone()));
	assert!(run(db, repair).await.is_none());
	assert_eq!(Outcome::decode(&global.get(MARKER).await?), Some(unfinished));
	assert_eq!(calls.get(), 4);
	global.remove(MARKER);

	assert!(
		run(db, || ready(Err!("candidate decode failure")))
			.await
			.is_none()
	);

	assert_absent(db, "global", MARKER, "interrupted run remains retryable").await;
	assert!(
		run(db, async || {
			global.get("unavailable_candidate").await?;
			Ok(Outcome::clean())
		})
		.await
		.is_none()
	);

	assert_eq!(stamp(db), Some(Outcome::clean()));
	assert_eq!(Outcome::decode(&global.get(MARKER).await?), Some(Outcome::clean()));
	global.remove(MARKER);

	assert!(!stands(db, || ready(Err!("family check failure"))).await);
	assert!(!stands(db, || ready(Ok(false))).await);
	assert_absent(db, "global", MARKER, "an unsettled check records nothing").await;
	assert!(stands(db, || ready(Ok(true))).await);
	assert_eq!(Outcome::decode(&global.get(MARKER).await?), Some(settled()));

	global.insert(MARKER, Outcome::clean().encode()?);
	let readonly_server = readonly_server(server)?;
	let readonly = Database::open(&readonly_server).await?;

	assert!(Boundary::writable(&readonly).is_none());
	assert!(stands(&readonly, || ready(Ok(false))).await, "a read-only open skips unchecked");
	let secondary_server = open_server(server, "rocksdb_secondary")?;
	let secondary = Database::open(&secondary_server).await?;

	assert!(secondary.engine.is_secondary());
	assert!(run(&secondary, repair).await.is_none());
	assert!(stamp(&secondary).is_none());
	assert_eq!(Outcome::decode(&secondary["global"].get(MARKER).await?), Some(Outcome::clean()));
	assert_eq!(calls.get(), 4);
	let rejected =
		Txn::insert(&readonly["global"], [(b"rejected_readonly", b"value")]).try_execute();

	assert!(matches!(rejected, Err(TxnError::Write(_))));
	assert_absent(&readonly, "global", b"rejected_readonly", "read-only write was rejected")
		.await;

	Ok(())
}

#[tokio::test]
#[tracing::instrument(level = "trace", skip_all)]
async fn ordinary_ladder_repairs_independent_work_and_retains_ambiguity() -> Result {
	let config = Figment::new().merge(("create_admin_room", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let db = &services.db;
	let global = &db["global"];

	migrations(services).await?;
	assert_eq!(Outcome::decode(&global.get(MARKER).await?), Some(Outcome::clean()));
	services
		.users
		.create(user_id!("@repair:localhost"), None, None)
		.await?;

	let retained = words(&[0, 201, 103]);

	seed_identity(db, EVENTS, b"$retained:example.org", 103);
	db["shortstatehash_statediff"].insert(&900_u64.to_be_bytes(), &retained);
	db["statehash_shortstatehash"].insert(b"retained", 900_u64.to_be_bytes());

	for old in [None, Some(b"declined".as_slice())] {
		global.remove(MARKER);
		global.remove(SUPERSEDED);
		if let Some(old) = old {
			global.insert(SUPERSEDED, old);
		}

		global.remove("clear_servername_status");
		db["eventid_shorteventid"].insert(b"$safe:example.org", 101_u64.to_be_bytes());
		db["shorteventid_eventid"].remove(&101_u64.to_be_bytes());
		db["shorteventid_eventid"].insert(&102_u64.to_be_bytes(), b"$safe:example.org");
		db["authchainkey_authchain"].insert(&102_u64.to_be_bytes(), 102_u64.to_be_bytes());
		migrations(services).await?;
		assert_stored(db, "global", SUPERSEDED, old.unwrap_or_default()).await?;
		assert_stored(db, "shorteventid_eventid", &101_u64.to_be_bytes(), b"$safe:example.org")
			.await?;

		assert_absent(db, "shorteventid_eventid", &102_u64.to_be_bytes(), "alias deleted").await;
		assert_stored(db, "shortstatehash_statediff", &900_u64.to_be_bytes(), &retained).await?;
		assert_stored(db, "global", "clear_servername_status", []).await?;
		let outcome =
			Outcome::decode(&global.get(MARKER).await?).expect("recognized final marker");

		assert_eq!(outcome.status, Status::Unfinished);
		assert!(
			count(&outcome, Shape::StatekeyOrphanEntry) > 0,
			"unprovable statekey orphan entry remains"
		);

		assert_eq!(
			count(&outcome, Shape::DeclinedMarker),
			0,
			"old markers do not become permanent residue"
		);

		assert!(outcome.valid());
		let marker = global.get(MARKER).await?.to_vec();

		db["eventid_shorteventid"].insert(b"$later:example.org", 104_u64.to_be_bytes());
		migrations(services).await?;
		assert_eq!(global.get(MARKER).await?.as_ref(), marker);
		assert_absent(
			db,
			"shorteventid_eventid",
			&104_u64.to_be_bytes(),
			"recognized unfinished honestly skips a new population",
		)
		.await;

		db["eventid_shorteventid"].remove(b"$later:example.org");
	}

	seed_identity(db, STATEKEYS, b"m.room.name\xff", 201);
	global.remove(MARKER);
	migrations(services).await?;
	assert_eq!(
		Outcome::decode(&global.get(MARKER).await?),
		Some(Outcome::clean()),
		"populated ordinary pipeline verifies clean"
	);

	assert_stored(db, "shortstatehash_statediff", &900_u64.to_be_bytes(), &retained).await?;

	Ok(())
}

#[tokio::test]
#[tracing::instrument(level = "trace", skip_all)]
async fn settled_databases_are_checked_once_and_legacy_markers_follow_the_record() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let db = &services.db;
	let global = &db["global"];
	let auth_chains = &db["authchainkey_authchain"];
	let event_id = b"$settled:example.org";
	let alias = 102_u64.to_be_bytes();

	seed_identity(db, EVENTS, event_id, 101);
	auth_chains.insert(&alias, alias);
	global.remove(MARKER);
	global.insert(SUPERSEDED, []);
	global.insert(CACHE_CLEARED, []);
	fix(services).await;
	assert_eq!(recorded(db).await, Some(settled()));
	assert_stored(db, "authchainkey_authchain", &alias, alias).await?;

	db["shorteventid_eventid"].insert(&alias, event_id);
	fix(services).await;
	assert_eq!(recorded(db).await, Some(settled()));
	assert_stored(db, "shorteventid_eventid", &alias, event_id).await?;

	global.remove(MARKER);
	fix(services).await;
	assert_eq!(recorded(db).await, Some(Outcome::clean()));
	assert_absent(db, "shorteventid_eventid", &alias, "a disagreeing family is repaired").await;
	assert_absent(db, "authchainkey_authchain", &alias, "the full repair clears the cache").await;

	auth_chains.insert(&alias, alias);
	global.remove(MARKER);
	global.remove(CACHE_CLEARED);
	fix(services).await;
	assert_eq!(recorded(db).await, Some(Outcome::clean()));
	assert_absent(db, "authchainkey_authchain", &alias, "an uncleared cache is repaired").await;
	assert_stored(db, "global", CACHE_CLEARED, []).await?;

	global.remove(SUPERSEDED);
	fix(services).await;
	assert_stored(db, "global", SUPERSEDED, []).await?;

	global.remove(MARKER);
	global.remove(SUPERSEDED);
	services.server.shutdown()?;
	fix(services).await;
	assert_absent(db, "global", MARKER, "an interrupted repair records no outcome").await;
	assert_absent(db, "global", SUPERSEDED, "an interrupted repair stays eligible").await;

	global.insert(SUPERSEDED, []);
	fix(services).await;
	assert_absent(db, "global", MARKER, "an interrupted check records no outcome").await;

	Ok(())
}

#[tokio::test]
#[tracing::instrument(level = "trace", skip_all)]
async fn family_check_rejects_malformed_mismatched_and_statekey_pairs() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let db = &services.db;
	let forward = &db["eventid_shorteventid"];
	let reverse = &db["shorteventid_eventid"];
	let malformed = b"not an event id";
	let event_id = b"$forward:example.org";
	let short = 105_u64.to_be_bytes();

	assert!(consistent(services).await?, "a fresh database agrees");
	seed_identity(db, EVENTS, malformed, 105);
	assert!(!consistent(services).await?, "a malformed pair fails its family");

	forward.remove(malformed);
	forward.insert(event_id, short);
	reverse.insert(&short, b"$reverse:example.org");
	assert!(!consistent(services).await?, "equal counts with different pairs disagree");

	forward.remove(event_id);
	reverse.remove(&short);
	seed_identity(db, STATEKEYS, b"m.room.name\xff", 201);
	assert!(consistent(services).await?, "paired rows agree");
	db["shortstatekey_statekey"].insert(&202_u64.to_be_bytes(), b"m.room.topic\xff");
	assert!(!consistent(services).await?, "a statekey disagreement fails");

	Ok(())
}

#[tokio::test]
#[tracing::instrument(level = "trace", skip_all)]
async fn final_verifier_reads_raw_state_and_preserves_uncertainty() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let db = &services.db;

	assert_eq!(verify(services, Uncertain::default()).await?, Outcome::clean());
	seed_identity(db, EVENTS, b"$event:example.org", 101);
	seed_identity(db, STATEKEYS, b"m.room.name\xff", 201);
	db["shortstatehash_statediff"].insert(&900_u64.to_be_bytes(), words(&[0, 201, 101]));
	db["statehash_shortstatehash"].insert(b"exposed", 900_u64.to_be_bytes());
	services
		.state_compressor
		.rows(900, None, None)
		.await?
		.count();

	let exposed = words(&[0, 201, 101, 0, 201, 101]);

	db["shortstatehash_statediff"].insert(&900_u64.to_be_bytes(), &exposed);
	let outcome = verify(services, Uncertain::default()).await?;

	assert_eq!(outcome.status, Status::Unfinished);
	assert_eq!(
		count(&outcome, Shape::DiffCollision),
		1,
		"raw diff collision survives healthy stale snapshot rows"
	);

	assert_stored(db, "shortstatehash_statediff", &900_u64.to_be_bytes(), &exposed).await?;

	assert!(
		db["shortstatehash_statemeta"]
			.get(&900_u64.to_be_bytes())
			.await
			.is_ok(),
		"read-only verification does not clear rows or repair"
	);
	db["shortstatehash_statediff"].remove(&900_u64.to_be_bytes());
	db["statehash_shortstatehash"].remove(b"exposed");
	assert_eq!(verify(services, Uncertain::default()).await?, Outcome::clean());
	let uncertain = verify(services, Uncertain {
		states: true,
		collected: true,
		rooms: true,
	})
	.await?;

	assert_eq!(uncertain.status, Status::Unfinished);
	let residual = [Shape::AffectedState, Shape::UnreachableState, Shape::PurgeResidue]
		.into_iter()
		.all(|shape| count(&uncertain, shape) > 0);

	assert!(
		residual,
		"completed-pass uncertainty cannot be erased by a clean residual census"
	);

	assert!(uncertain.valid());
	Ok(())
}

#[test]
fn reference_framing_is_checked_before_absence() {
	let row = words(&[0, 9, 10, 0, 11, 12]);

	assert_eq!(entries(&row).collect::<Result<Vec<_>>>().unwrap(), [(9, 10), (11, 12)]);
	for malformed in [words(&[0, 0]), words(&[0, 9]), words(&[0, 0, 0, 10]), vec![0; 7]] {
		entries(&malformed)
			.collect::<Result<Vec<_>>>()
			.expect_err("malformed framing cannot prove absence");
	}
}

#[tokio::test]
#[tracing::instrument(level = "trace", skip_all)]
async fn identity_heals_and_cleanup_are_candidate_scoped() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let db = &services.db;
	let event_id = |name| format!("${name}:example.org");
	let aliases = [(112_u64, 113_u64, "disagree"), (114, 115, "claimed")];

	db["eventid_shorteventid"].insert(&event_id("dangling"), 101_u64.to_be_bytes());
	db["shorteventid_eventid"].insert(&102_u64.to_be_bytes(), event_id("promote"));
	db["eventid_shorteventid"].insert(&event_id("alias"), 103_u64.to_be_bytes());
	db["shorteventid_eventid"].insert(&103_u64.to_be_bytes(), event_id("alias"));
	db["shorteventid_eventid"].insert(&104_u64.to_be_bytes(), event_id("alias"));
	db["shorteventid_eventid"].insert(&105_u64.to_be_bytes(), event_id("ambiguous"));
	db["shorteventid_eventid"].insert(&106_u64.to_be_bytes(), event_id("ambiguous"));
	db["eventid_shorteventid"].insert(&event_id("other"), 106_u64.to_be_bytes());
	db["eventid_shorteventid"].insert(&event_id("zero"), 0_u64.to_be_bytes());
	db["eventid_shorteventid"].insert(&event_id("double1"), 111_u64.to_be_bytes());
	db["eventid_shorteventid"].insert(&event_id("double2"), 111_u64.to_be_bytes());
	for (loser, winner, name) in aliases {
		db["eventid_shorteventid"].insert(&event_id(name), winner.to_be_bytes());
		db["shorteventid_eventid"].insert(&loser.to_be_bytes(), event_id(name));
		db["shorteventid_eventid"].insert(&winner.to_be_bytes(), event_id(name));
	}

	db["shorteventid_eventid"].insert(&113_u64.to_be_bytes(), event_id("wrong"));
	db["eventid_shorteventid"].insert(&event_id("additional"), 115_u64.to_be_bytes());
	db["statekey_shortstatekey"].insert(b"m.room.name\xff", 202_u64.to_be_bytes());
	db["statekey_shortstatekey"].insert(b"invalid", b"malformed");
	db["shortstatekey_statekey"].insert(&201_u64.to_be_bytes(), b"invalid");
	db["global"].insert("fix_short_injectivity", []);
	db["global"].insert("clear_auth_chain_cache", []);
	db["authchainkey_authchain"].insert(&104_u64.to_be_bytes(), 104_u64.to_be_bytes());
	for short in CACHED {
		db["authchainkey_authchain"].insert(&short.to_be_bytes(), short.to_be_bytes());
	}

	let retained_forward = db["statekey_shortstatekey"]
		.get(b"invalid")
		.await?
		.to_vec();

	let retained_reverse = db["shortstatekey_statekey"]
		.get(&201_u64.to_be_bytes())
		.await?
		.to_vec();

	let identities = repair_identities(services).await?;

	assert_stored(db, "shorteventid_eventid", &101_u64.to_be_bytes(), event_id("dangling"))
		.await?;

	assert_stored(db, "eventid_shorteventid", &event_id("promote"), 102_u64.to_be_bytes())
		.await?;

	assert_absent(db, "shorteventid_eventid", &104_u64.to_be_bytes(), "unused alias deleted")
		.await;

	assert_absent(db, "eventid_shorteventid", &event_id("ambiguous"), "all reverse claims count")
		.await;

	assert_absent(db, "shorteventid_eventid", &0_u64.to_be_bytes(), "zero never healed").await;
	assert_stored(db, "statekey_shortstatekey", b"invalid", &retained_forward).await?;
	assert_stored(db, "shortstatekey_statekey", &201_u64.to_be_bytes(), &retained_reverse)
		.await?;

	assert!(identities.statekeys.malformed > 0);
	assert_stored(db, "shortstatekey_statekey", &202_u64.to_be_bytes(), b"m.room.name\xff")
		.await?;

	assert_absent(
		db,
		"shorteventid_eventid",
		&111_u64.to_be_bytes(),
		"ambiguous forward claim never healed",
	)
	.await;

	for (loser, _, name) in aliases {
		assert_stored(db, "shorteventid_eventid", &loser.to_be_bytes(), event_id(name)).await?;
	}

	assert!(
		identities
			.events
			.candidates
			.iter()
			.any(|candidate| candidate.short == 105)
	);

	assert_absent(
		db,
		"authchainkey_authchain",
		&104_u64.to_be_bytes(),
		"old-stamped cache cleared",
	)
	.await;

	for short in CACHED {
		assert_absent(
			db,
			"authchainkey_authchain",
			&short.to_be_bytes(),
			"every deletion batch cleared",
		)
		.await;
	}

	assert_absent(db, "global", MARKER, "partial repair does not stamp").await;

	Ok(())
}

#[tokio::test]
#[tracing::instrument(level = "trace", skip_all)]
async fn each_alias_residence_protects_its_reverse_row() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let db = &services.db;
	let event_id = |short: u64| format!("$residence{short}:example.org");
	let statekey = |short: u64| {
		[b"m.room.member\xff".as_slice(), format!("@{short}:example.org").as_bytes()].concat()
	};

	seed_aliases(db, EVENTS, 301..=307, event_id);
	seed_aliases(db, STATEKEYS, 501..=502, statekey);
	db["shortstatehash_statediff"]
		.insert(&701_u64.to_be_bytes(), words(&[0, 501, 301, 0, 502, 302]));

	db["shorteventid_shortstatehash"].insert(&303_u64.to_be_bytes(), 701_u64.to_be_bytes());
	let relation = relation_key();

	db["relatesto_typed"].insert(&relation, 304_u64.to_be_bytes());
	db["authchainkey_authchain"].insert(&305_u64.to_be_bytes(), 306_u64.to_be_bytes());
	let identities = identity_census(services).await?;
	let references = References::census(services, &identities).await?;

	assert!(references.events.contains(&305));
	assert!(references.events.contains(&306));
	let identities = repair_identities(services).await?;

	for short in 301_u64..=304 {
		assert_stored(db, "shorteventid_eventid", &short.to_be_bytes(), event_id(short)).await?;
	}

	for short in 305_u64..=307 {
		assert_absent(
			db,
			"shorteventid_eventid",
			&short.to_be_bytes(),
			"cache refs durably removed or alias unused",
		)
		.await;
	}

	for short in 501_u64..=502 {
		assert_stored(db, "shortstatekey_statekey", &short.to_be_bytes(), statekey(short))
			.await?;
	}

	assert!(
		identities
			.events
			.candidates
			.iter()
			.any(|candidate| matches!(candidate.kind, Kind::Alias(_)))
	);

	seed_aliases(db, EVENTS, 308..=310, event_id);
	seed_aliases(db, STATEKEYS, 503..=504, statekey);
	let malformed_key_state = words(&[0, 503, 308]);

	db["shortstatehash_statediff"].insert(b"malformed_hash", &malformed_key_state);
	db["relatesto_typed"].insert(b"malformed_key", 309_u64.to_be_bytes());
	repair_identities(services).await?;
	for short in [308_u64, 309] {
		assert_stored(db, "shorteventid_eventid", &short.to_be_bytes(), event_id(short)).await?;
	}

	assert_absent(
		db,
		"shorteventid_eventid",
		&310_u64.to_be_bytes(),
		"unrelated event alias deleted",
	)
	.await;

	assert_stored(db, "shortstatekey_statekey", &503_u64.to_be_bytes(), statekey(503)).await?;
	assert_absent(
		db,
		"shortstatekey_statekey",
		&504_u64.to_be_bytes(),
		"unrelated statekey alias deleted",
	)
	.await;

	assert_stored(db, "shortstatehash_statediff", b"malformed_hash", &malformed_key_state)
		.await?;

	assert_stored(db, "relatesto_typed", b"malformed_key", 309_u64.to_be_bytes()).await?;
	db["shorteventid_eventid"].insert(&311_u64.to_be_bytes(), event_id(311));
	db["eventid_shorteventid"].insert(&event_id(311), 411_u64.to_be_bytes());
	db["shorteventid_eventid"].insert(&411_u64.to_be_bytes(), event_id(311));
	db["relatesto_typed"].insert(b"malformed", b"malformed");
	let identities = repair_identities(services).await?;
	let references = References::census(services, &identities).await?;

	assert!(!references.event_complete);
	assert!(references.statekey_complete);
	assert_stored(db, "shorteventid_eventid", &311_u64.to_be_bytes(), event_id(311)).await?;

	Ok(())
}

#[test]
fn marker_decode_contract() -> Result {
	let clean = Outcome::clean();
	let bytes = clean.encode()?;

	assert_eq!(Outcome::decode(&bytes), Some(clean));

	let bytes = settled().encode()?;
	let residue = br#"{"version":1,"status":"superseded","counts":[1,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"samples":[],"truncated":false}"#;

	assert_eq!(Outcome::decode(&bytes), Some(settled()));
	assert!(Outcome::decode(residue).is_none());
	Outcome { truncated: true, ..settled() }
		.encode()
		.expect_err("a settlement carries no residue");

	// A legacy decline record must never read as this repair's completion.
	assert_ne!(MARKER, "fix_short_injectivity");
	assert!(Outcome::decode(b"").is_none());
	assert!(Outcome::decode(b"declined").is_none());
	assert!(Outcome::decode(br#"{"version":2,"status":"clean","counts":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"samples":[],"truncated":false}"#).is_none());
	assert!(Outcome::decode(br#"{"version":1,"status":"clean"}"#).is_none());

	let sample = Sample {
		shape: Shape::DiffCollision,
		identity: b"state".as_slice().into(),
		reason: Reason::HistoricalState,
	};

	let unfinished = Outcome {
		status: Status::Unfinished,
		counts: [1; 15],
		samples: [sample.clone()].into(),
		truncated: true,
		..Outcome::clean()
	};

	let bytes = unfinished.encode()?;

	assert_eq!(Outcome::decode(&bytes), Some(unfinished.clone()));
	let samples = repeat_with(|| sample.clone())
		.take(SAMPLE_LIMIT + 1)
		.collect();

	let oversampled = Outcome { samples, ..unfinished };

	oversampled
		.encode()
		.expect_err("diagnostic sample cap");

	let samples = [Sample {
		identity: [0; IDENTITY_LIMIT + 1].as_slice().into(),
		..sample
	}]
	.into();

	let oversized = Outcome { samples, ..unfinished };

	oversized
		.encode()
		.expect_err("diagnostic identity byte cap");

	Ok(())
}

#[tokio::test]
#[tracing::instrument(level = "trace", skip_all)]
async fn malformed_forward_value_prefixes_withhold_identity_writes() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let db = &services.db;
	let hidden = |short: u64| format!("$hidden{short}:example.org");
	let claimants = [601_u64, 602, 603, 606];
	let aliases = [
		(603_u64, 604_u64, b"$loser:example.org".as_slice()),
		(605, 606, b"$winner:example.org"),
	];

	db["eventid_shorteventid"].insert(b"$dangling:example.org", words(&[601]));
	db["shorteventid_eventid"].insert(&602_u64.to_be_bytes(), b"$promote:example.org");
	for (loser, winner, event_id) in aliases {
		db["eventid_shorteventid"].insert(event_id, words(&[winner]));
		db["shorteventid_eventid"].insert(&winner.to_be_bytes(), event_id);
		db["shorteventid_eventid"].insert(&loser.to_be_bytes(), event_id);
	}

	for short in claimants {
		db["eventid_shorteventid"].insert(&hidden(short), [words(&[short]), vec![99]].concat());
	}

	db["roomid_spacehierarchy"].insert(SPACE, b"cached summary");
	repair_identities(services).await?;
	assert_stored(db, "roomid_spacehierarchy", SPACE, b"cached summary").await?;
	assert_absent(
		db,
		"shorteventid_eventid",
		&601_u64.to_be_bytes(),
		"extra prefix claimant withholds the dangling winner heal",
	)
	.await;

	assert_absent(
		db,
		"eventid_shorteventid",
		b"$promote:example.org",
		"prefix claimant withholds the reverse row promotion",
	)
	.await;

	for (loser, _, event_id) in aliases {
		assert_stored(db, "shorteventid_eventid", &loser.to_be_bytes(), event_id).await?;
	}

	for short in claimants {
		db["eventid_shorteventid"].remove(&hidden(short));
	}

	repair_identities(services).await?;
	assert_absent(db, "roomid_spacehierarchy", SPACE, "admitted heals invalidate summaries")
		.await;

	assert_stored(db, "shorteventid_eventid", &601_u64.to_be_bytes(), b"$dangling:example.org")
		.await?;

	assert_stored(db, "eventid_shorteventid", b"$promote:example.org", 602_u64.to_be_bytes())
		.await?;

	for (loser, ..) in aliases {
		assert_absent(
			db,
			"shorteventid_eventid",
			&loser.to_be_bytes(),
			"unclaimed alias deleted once the prefix claimants are gone",
		)
		.await;
	}

	Ok(())
}

#[tokio::test]
#[tracing::instrument(level = "trace", skip_all)]
async fn stop_request_interrupts_identity_and_reference_scans() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let db = &services.db;
	let event_id = |short: u64| format!("$stopped{short}:example.org");

	seed_aliases(db, EVENTS, 801..=801, event_id);
	db["shortstatehash_statediff"].insert(&701_u64.to_be_bytes(), words(&[0, 1, 2]));
	let identities = identity_census(services).await?;

	services.server.shutdown()?;
	assert!(
		interrupted(References::census(services, &identities).await),
		"reference scan honors a stop request"
	);

	assert!(
		interrupted(repair_identities(services).await),
		"stopped repair reports interruption"
	);

	assert_stored(db, "shorteventid_eventid", &801_u64.to_be_bytes(), event_id(801)).await?;
	// With the alias gone no claim scan runs, so only the row scan can observe the stop.
	db["shorteventid_eventid"].remove(&801_u64.to_be_bytes());
	assert!(
		interrupted(identity_census(services).await),
		"identity scan honors a stop request"
	);

	Ok(())
}

#[tokio::test]
#[tracing::instrument(level = "trace", skip_all)]
async fn identity_only_heal_invalidates_space_summaries() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let db = &services.db;
	let space = room_id!("!space:example.org");

	seed_identity(db, STATEKEYS, b"m.space.child\xff!child:example.org", 21);
	db["eventid_shorteventid"].insert(b"$child:example.org", 31_u64.to_be_bytes());
	db["shortstatehash_statediff"].insert(&41_u64.to_be_bytes(), words(&[0, 21, 31]));
	db["roomid_shortstatehash"].insert(space, 41_u64.to_be_bytes());
	db["roomid_spacehierarchy"].insert(space, b"cached summary");
	assert_eq!(space_child_count(services, space).await?, 0, "unresolvable child is omitted");

	repair_identities(services).await?;
	assert_absent(db, "roomid_spacehierarchy", space, "heal-only repair clears summaries").await;
	assert_stored(db, "shorteventid_eventid", &31_u64.to_be_bytes(), b"$child:example.org")
		.await?;

	assert_eq!(space_child_count(services, space).await?, 1, "healed child is enumerated");

	Ok(())
}

async fn assert_absent<K>(db: &Database, map: &str, key: &K, message: &str)
where
	K: AsRef<[u8]> + Debug + Sync + ?Sized,
{
	assert!(db[map].get(key).await.is_missing(), "{message}");
}

fn readonly_server(server: &Server) -> Result<Arc<Server>> {
	open_server(server, "rocksdb_read_only")
}

fn open_server(server: &Server, mode: &str) -> Result<Arc<Server>> {
	let raw = Figment::new()
		.merge(("server_name", server.config.server_name.as_str()))
		.merge(("database_path", &server.config.database_path))
		.merge((mode, true));

	let config = Config::new(&raw)?;
	let logging = Logging {
		subscriber: server.log.subscriber.clone(),
		reload: LogLevelReloadHandles::default(),
		capture: server.log.capture.clone(),
	};

	let server = Server::new(
		config,
		Sources::default(),
		Some(server.runtime()),
		logging,
		server.metrics.clone(),
	);

	Ok(Arc::new(server))
}

fn words(values: &[u64]) -> Vec<u8> {
	values
		.iter()
		.copied()
		.flat_map(u64::to_be_bytes)
		.collect()
}

fn seed_identity(db: &Database, (forward, reverse): (&str, &str), id: &[u8], short: u64) {
	db[forward].insert(id, short.to_be_bytes());
	db[reverse].insert(&short.to_be_bytes(), id);
}

async fn assert_stored<K>(
	db: &Database,
	map: &str,
	key: &K,
	expected: impl AsRef<[u8]> + Send,
) -> Result
where
	K: AsRef<[u8]> + Debug + Sync + ?Sized,
{
	let stored = db[map].get(key).await?;

	assert_eq!(stored.as_ref(), expected.as_ref());
	Ok(())
}

fn count(outcome: &Outcome, shape: Shape) -> u64 {
	shape
		.slot()
		.and_then(|slot| outcome.counts.get(slot))
		.copied()
		.expect("every shape has a count slot")
}

async fn recorded(db: &Database) -> Option<Outcome> {
	db["global"]
		.get(MARKER)
		.await
		.ok()
		.as_deref()
		.and_then(Outcome::decode)
}

fn settled() -> Outcome {
	Outcome {
		status: Status::Superseded,
		..Outcome::clean()
	}
}

fn seed_aliases<I>(
	db: &Database,
	(forward, reverse): (&str, &str),
	shorts: RangeInclusive<u64>,
	identity: impl Fn(u64) -> I,
) where
	I: AsRef<[u8]>,
{
	for short in shorts {
		let winner = short
			.checked_add(100)
			.expect("fixture short is bounded");

		db[forward].insert(&identity(short), winner.to_be_bytes());
		db[reverse].insert(&winner.to_be_bytes(), identity(short));
		db[reverse].insert(&short.to_be_bytes(), identity(short));
	}
}

fn relation_key() -> Vec<u8> { [words(&[1, 2]), vec![1], words(&[3, 4])].concat() }

fn interrupted<T>(result: Result<T>) -> bool { result.is_err_and(|error| error.is_interrupted()) }

async fn space_child_count(services: &Services, space: &RoomId) -> Result<usize> {
	services
		.state_accessor
		.room_state_keys_with_ids(space, &StateEventType::SpaceChild)
		.ready_try_fold(0_usize, |count, _| Ok(count.saturating_add(1)))
		.await
}
