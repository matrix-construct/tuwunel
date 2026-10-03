use std::{iter::once, sync::Arc};

use futures::TryStreamExt;
use ruma::{EventId, RoomId, event_id, events::StateEventType, room_id, server_name, user_id};
use serde_json::{Value, from_slice, from_value, json, to_vec};
use tuwunel_core::{
	PduEvent, Result,
	config::Figment,
	matrix::{PduCount, PduId, RawPduId},
	utils::{BoolExt, IterStream, result::NotFound},
};
use tuwunel_database::{Database, KeyBuf, SEP, serialize_key, serialize_val};

use super::{inspect, repair};
use crate::{Services, test_utils::fixture};

type Record = (&'static str, Vec<u8>, Vec<u8>);
type Records = Vec<Record>;

const LOSER: u64 = 60_000;
const WINNER: u64 = 70_000;
const LEFT: [&str; 2] = ["roomuserid_leftcount", "userroomid_leftstate"];
const COLUMNS: [&str; 11] = [
	"pduid_pdu",
	"eventid_pduid",
	"tokenids",
	"relatesto_typed",
	"threadid_userids",
	"threadrootid_latestcount",
	"threadactivityid_rootid",
	"useridcount_notification",
	"servernameevent_data",
	"servercurrentevent_data",
	"servershortroomid_park",
];

#[tokio::test]
async fn ordinary_nonforce_purge_leaves_proven_residue_and_local_leave_records() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let db = &services.db;
	let room = room_id!("!repair:localhost");

	live(services, room).await;
	let residue = residences(LOSER, room)?;

	store(db, &residue);
	let canonical = event_id!("$canonical:localhost");
	let id = packed(WINNER, PduCount::Normal(20));

	db["pduid_pdu"].insert(&id, to_vec(&event(canonical, room))?);
	db["eventid_pduid"].insert(canonical, id.as_bytes());
	services
		.state_cache
		.mark_as_left(user_id!("@local:localhost"), room, PduCount::Normal(21));

	let left = snapshot(db, &LEFT).await?;

	assert_eq!(left.len(), 2);
	let timestamp = serialize_key((room, 1_u64, 2_u64))?;
	let count = 11_u64.to_be_bytes();

	db["roomid_tscount_pducount"].insert(&timestamp, count);
	let lock = services.state.mutex.lock(room).await;

	services
		.delete
		.delete_room(room, false, lock)
		.await?;

	assert_records(db, &residue).await?;
	assert_eq!(snapshot(db, &LEFT).await?, left);
	assert_absent(db, "roomid_shortroomid", room.as_bytes(), "ordinary purge removes owner")
		.await;

	assert_absent(
		db,
		"roomid_shortstatehash",
		room.as_bytes(),
		"ordinary purge removes state pointer",
	)
	.await;

	assert_eq!(db["roomid_pduleaves"].count().await, 0);
	assert_eq!(inspect(services).await?.unfinished, [LOSER].into());
	let result = repair(services).await?;

	assert_eq!((result.deleted, result.moved), (1, 0));
	assert!(result.unfinished.is_empty());
	assert!(!result.unknown);
	assert_eq!(snapshot(db, &COLUMNS).await?, Records::new());
	assert_eq!(snapshot(db, &LEFT).await?, left);
	let retained = db["roomid_tscount_pducount"]
		.get(&timestamp)
		.await?;

	assert_eq!(retained.as_ref(), count);

	assert!(inspect(services).await?.unfinished.is_empty());
	assert_eq!(repair(services).await?.deleted, 0);
	Ok(())
}

#[tokio::test]
async fn move_rewrites_every_residence_and_normal_redaction_and_purge_use_the_winner() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let db = &services.db;
	let room = room_id!("!repair:localhost");

	live(services, room).await;
	let source = residences(LOSER, room)?;
	let expected = residences(WINNER, room)?;

	store(db, &source);

	// Leave the search key destination absent so the move itself must write it.
	relocated(&source, &expected)
		.filter(|(_, (column, ..))| *column != "tokenids")
		.for_each(|(_, (column, key, value))| {
			let column: &str = column;

			db[column].insert(key, value);
		});

	let result = repair(services).await?;
	let moved = packed(WINNER, PduCount::Normal(11));
	let search_key = token(WINNER, &moved);

	assert_eq!((result.deleted, result.moved), (0, 1));
	assert!(result.unfinished.is_empty());
	assert!(!result.unknown);
	assert!(db["tokenids"].get(&search_key).await.is_ok(), "move writes the search key");
	assert_records(db, &expected).await?;
	for ((column, key, _), _) in relocated(&source, &expected) {
		assert_absent(db, column, key, "old residence removed").await;
	}

	let normal = event_id!("$normal:localhost");
	let backfill = event_id!("$backfill:localhost");
	let user = user_id!("@local:localhost");
	let mappings = [(normal, moved), (backfill, packed(WINNER, PduCount::Backfilled(-4)))];

	for (event_id, pdu_id) in mappings {
		assert_eq!(services.timeline.get_pdu_id(event_id).await?, pdu_id, "{event_id}");
	}

	let event_ids: Vec<_> = services
		.timeline
		.pdus(None, room, None)
		.map_ok(|(_, pdu)| pdu.event_id)
		.try_collect()
		.await?;

	assert_eq!(event_ids, [backfill.to_owned(), normal.to_owned()]);
	assert!(
		services
			.threads
			.user_participated(normal, user)
			.await
	);

	let key = serialize_key((user, 11_u64))?;
	let notification = db["useridcount_notification"].get(&key).await?;

	let notification: Value = from_slice(&notification)?;

	assert_eq!(notification["extension"], json!({"preserved": [1, "opaque"]}));
	assert_eq!(notification["sroomid"], WINNER);
	db["roomid_shortstatehash"].remove(room);
	create_state(services, room).await?;
	let reason: PduEvent = from_value(event(event_id!("$redaction:localhost"), room))?;
	let lock = services.state.mutex.lock(room).await;

	services
		.timeline
		.redact_pdu(normal, &reason, WINNER, &lock)
		.await?;

	let redacted = services.timeline.get_pdu_json(normal).await?;

	assert!(
		redacted["content"]
			.as_object()
			.expect("redacted content")
			.contains_key("body")
			.is_false()
	);

	assert_absent(
		db,
		"pduid_pdu",
		packed(LOSER, PduCount::Normal(11)).as_bytes(),
		"no stale unredacted copy",
	)
	.await;

	assert_eq!(db["tokenids"].count().await, 0);
	assert_eq!(repair(services).await?.moved, 0, "normal redaction leaves no loser to recensus");
	services
		.delete
		.delete_room(room, false, lock)
		.await?;

	for (event_id, _) in mappings {
		assert!(
			services
				.timeline
				.get_pdu_id(event_id)
				.await
				.is_missing(),
			"normal purge removed the moved mapping of {event_id}"
		);
	}

	for column in [
		"pduid_pdu",
		"threadactivityid_rootid",
		"threadrootid_latestcount",
		"threadid_userids",
	] {
		assert_eq!(db[column].count().await, 0, "{column}");
	}

	let retained = snapshot(db, &COLUMNS).await?;
	let result = repair(services).await?;

	assert_eq!((result.deleted, result.moved), (0, 0));
	assert!(
		result.unfinished.contains(&WINNER),
		"ordinary purge leaves queues and notification rows without PDU attribution"
	);

	assert_eq!(snapshot(db, &COLUMNS).await?, retained);
	Ok(())
}

#[tokio::test]
async fn conflicting_destination_preserves_both_sides_then_recensus_observes_normal_changes()
-> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let db = &services.db;
	let room = room_id!("!repair:localhost");

	live(services, room).await;
	let source = residences(LOSER, room)?;
	let destination = residences(WINNER, room)?;

	store(db, &source);
	let (column, key, _) = destination
		.iter()
		.find(|(column, ..)| *column == "threadid_userids")
		.expect("thread fixture");

	let column: &str = column;

	db[column].insert(key, b"@other:localhost");
	let before = snapshot(db, &COLUMNS).await?;
	let result = repair(services).await?;

	assert_eq!(result.moved, 0);
	assert!(result.unfinished.contains(&LOSER));
	assert_eq!(snapshot(db, &COLUMNS).await?, before);
	db[column].remove(key);
	services
		.search
		.deindex_pdu(LOSER, &packed(LOSER, PduCount::Normal(11)), "preserve");

	let result = repair(services).await?;

	assert_eq!(result.moved, 1);
	assert!(result.unfinished.is_empty());
	assert_eq!(
		db["tokenids"].count().await,
		0,
		"fresh census does not resurrect removed search keys"
	);

	let expected: Records = destination
		.into_iter()
		.filter(|(column, ..)| *column != "tokenids")
		.collect();

	assert_records(db, &expected).await?;
	assert_eq!(repair(services).await?.moved, 0);
	Ok(())
}

#[tokio::test]
async fn unsupported_ownership_and_liveness_stay_byte_identical() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let db = &services.db;
	let room = room_id!("!repair:localhost");

	for case in [
		"no pdu",
		"mixed pdu",
		"torn mapping",
		"mixed search",
		"mixed thread",
		"reverse mixed thread",
		"malformed pdu",
		"notification key tail",
		"missing state",
		"missing leaves",
		"purge with state",
		"purge with leaves",
		"claimed winner",
		"dangling mapping",
	] {
		live(services, room).await;
		store(db, &residences(LOSER, room)?);
		unsupported(db, room, case)?;

		let before = snapshot(db, &COLUMNS).await?;

		store(db, &independent_residue()?);
		let result = repair(services).await?;

		assert_eq!((result.deleted, result.moved), (1, 0), "{case}");
		assert!(result.unfinished.contains(&LOSER), "{case}");
		assert_eq!(snapshot(db, &COLUMNS).await?, before, "{case}");
		erase(db, &before);
		db["roomid_shortroomid"].remove(room_id!("!other:localhost"));
	}

	Ok(())
}

fn independent_residue() -> Result<Records> {
	let independent = event_id!("$independent:localhost");
	let id = packed(LOSER + 1, PduCount::Normal(30));
	let residue: Records = [
		(
			"pduid_pdu",
			id.as_bytes().to_vec(),
			to_vec(&event(independent, room_id!("!purged:localhost")))?,
		),
		("eventid_pduid", independent.as_bytes().to_vec(), id.as_bytes().to_vec()),
	]
	.into();

	Ok(residue)
}

#[tokio::test]
async fn unidentified_residue_withholds_purges_and_unreadable_owners_withhold_all() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let db = &services.db;
	let room = room_id!("!repair:localhost");

	for (column, key, value) in zero_hints() {
		live(services, room).await;
		let expected = residences(WINNER, room)?;
		let residue = independent_residue()?;

		store(db, &residences(LOSER, room)?);
		store(db, &residue);
		db[column].insert(&key, &value);
		let inspected = inspect(services).await?;
		let result = repair(services).await?;

		assert!(inspected.unknown && result.unknown, "{column} zero hint is unidentified");
		assert_eq!((result.deleted, result.moved), (0, 1), "{column}");
		assert!(!result.unfinished.contains(&LOSER), "{column} move proceeds");
		assert!(result.unfinished.contains(&(LOSER + 1)), "{column} purge withheld");
		assert_records(db, &expected).await?;
		assert_records(db, &residue).await?;
		assert_eq!(db[column].get(&key).await?.as_ref(), value.as_slice(), "{column}");
		erase(db, &snapshot(db, &COLUMNS).await?);
	}

	live(services, room).await;
	store(db, &residences(LOSER, room)?);
	db["roomid_shortroomid"].insert(room_id!("!unreadable:localhost"), [1, 2, 3]);
	let before = snapshot(db, &COLUMNS).await?;
	let result = repair(services).await?;

	assert!(result.unknown);
	assert_eq!((result.deleted, result.moved), (0, 0));
	assert!(result.unfinished.contains(&LOSER));
	assert_eq!(snapshot(db, &COLUMNS).await?, before);
	Ok(())
}

fn zero_hints() -> Records {
	let root = packed(0, PduCount::Normal(11));
	let thread_key = packed(0, PduCount::Normal(12))
		.as_bytes()
		.to_vec();

	[
		("tokenids", token(0, &root), Vec::new()),
		("threadactivityid_rootid", thread_key, root.as_bytes().to_vec()),
		("useridcount_notification", b"zero-hint".to_vec(), br#"{"sroomid":0}"#.to_vec()),
	]
	.into()
}

fn unsupported(db: &Database, room: &RoomId, case: &str) -> Result {
	match case {
		| "no pdu" => {
			db["pduid_pdu"].remove(&packed(LOSER, PduCount::Normal(11)));
			db["pduid_pdu"].remove(&packed(LOSER, PduCount::Backfilled(-4)));
		},
		| "mixed pdu" => db["pduid_pdu"].insert(
			&packed(LOSER, PduCount::Backfilled(-4)),
			to_vec(&event(event_id!("$backfill:localhost"), room_id!("!other:localhost")))?,
		),
		| "torn mapping" => db["eventid_pduid"].insert(
			event_id!("$backfill:localhost"),
			packed(LOSER, PduCount::Normal(11)).as_bytes(),
		),
		| "mixed search" => {
			let key = token(LOSER, &packed(LOSER, PduCount::Normal(11)));

			db["tokenids"].remove(&key);
			db["tokenids"].insert(&token(LOSER, &packed(WINNER, PduCount::Normal(11))), []);
		},
		| "mixed thread" => db["threadactivityid_rootid"].insert(
			&packed(LOSER, PduCount::Normal(12)),
			packed(WINNER, PduCount::Normal(11)).as_bytes(),
		),
		| "reverse mixed thread" => db["threadactivityid_rootid"].insert(
			&packed(WINNER, PduCount::Normal(12)),
			packed(LOSER, PduCount::Normal(11)).as_bytes(),
		),
		| "malformed pdu" =>
			db["pduid_pdu"].insert(&packed(LOSER, PduCount::Backfilled(-4)), b"{bad json"),
		| "notification key tail" => {
			let (column, key, value) = residences(LOSER, room)?
				.into_iter()
				.find(|(column, ..)| column.eq(&"useridcount_notification"))
				.expect("notification fixture row");

			let key = [key.as_slice(), b"tail"].concat();

			db[column].insert(&key, value);
		},
		| "missing state" => db["roomid_shortstatehash"].remove(room),
		| "missing leaves" => db["roomid_pduleaves"].remove(&leaves_key(room)?),
		| "purge with state" => {
			db["roomid_shortroomid"].remove(room);
			db["roomid_pduleaves"].remove(&leaves_key(room)?);
		},
		| "purge with leaves" => {
			db["roomid_shortroomid"].remove(room);
			db["roomid_shortstatehash"].remove(room);
		},
		| "claimed winner" =>
			db["roomid_shortroomid"].insert(room_id!("!other:localhost"), WINNER.to_be_bytes()),
		| "dangling mapping" => db["eventid_pduid"].insert(
			event_id!("$dangling:localhost"),
			packed(LOSER, PduCount::Normal(13)).as_bytes(),
		),
		| _ => unreachable!(),
	}

	Ok(())
}

fn leaves_key(room: &RoomId) -> Result<KeyBuf> {
	serialize_key((room, event_id!("$normal:localhost")))
}

async fn live(services: &Services, room: &RoomId) {
	services.db["roomid_shortroomid"].insert(room, WINNER.to_be_bytes());
	services.db["roomid_shortstatehash"].insert(room, 99_u64.to_be_bytes());
	let lock = services.state.mutex.lock(room).await;

	services
		.state
		.set_forward_extremities(room, once(event_id!("$normal:localhost")), &lock)
		.await;
}

fn residences(short: u64, room: &RoomId) -> Result<Records> {
	let normal = packed(short, PduCount::Normal(11));
	let backfill = packed(short, PduCount::Backfilled(-4));
	let normal_id = event_id!("$normal:localhost");
	let backfill_id = event_id!("$backfill:localhost");
	let activity = packed(short, PduCount::Normal(12));
	let notification = json!({"ts": 1, "sroomid": short, "actions": [], "extension": {"preserved": [1, "opaque"]}});
	let relation = [
		short.to_be_bytes().as_slice(),
		11_u64.to_be_bytes().as_slice(),
		&[1],
		1_u64.to_be_bytes().as_slice(),
		12_u64.to_be_bytes().as_slice(),
	]
	.concat();

	let queues = [
		("servernameevent_data", "a.example"),
		("servernameevent_data", "+application-service"),
		("servernameevent_data", "$@local:localhost"),
		("servercurrentevent_data", "long-destination.example"),
		("servercurrentevent_data", "+another-app"),
		("servercurrentevent_data", "$@different:localhost"),
	];

	let normal_bytes = || normal.as_bytes().to_vec();
	let backfill_bytes = || backfill.as_bytes().to_vec();
	let rows = vec![
		("pduid_pdu", normal_bytes(), to_vec(&event(normal_id, room))?),
		("pduid_pdu", backfill_bytes(), to_vec(&event(backfill_id, room))?),
		("eventid_pduid", normal_id.as_bytes().to_vec(), normal_bytes()),
		("eventid_pduid", backfill_id.as_bytes().to_vec(), backfill_bytes()),
		("tokenids", token(short, &normal), Vec::new()),
		("relatesto_typed", relation, 123_u64.to_be_bytes().to_vec()),
		("threadid_userids", normal_bytes(), b"@local:localhost".to_vec()),
		("threadrootid_latestcount", normal_bytes(), 12_u64.to_be_bytes().to_vec()),
		("threadactivityid_rootid", activity.as_bytes().to_vec(), normal_bytes()),
		(
			"useridcount_notification",
			serialize_key((user_id!("@local:localhost"), 11_u64))?.to_vec(),
			to_vec(&notification)?,
		),
		(
			"servershortroomid_park",
			serialize_key((server_name!("park.example"), short))?.to_vec(),
			serialize_val((123_u64, 2_u64))?.to_vec(),
		),
	];

	let queue_rows = queues.into_iter().map(|(column, destination)| {
		let prefix = if destination.starts_with('$') {
			[destination.as_bytes(), &[SEP], b"push-key", &[SEP]].concat()
		} else {
			[destination.as_bytes(), &[SEP]].concat()
		};

		let key = [prefix.as_slice(), backfill.as_bytes()].concat();

		(column, key, Vec::new())
	});

	let records = rows.into_iter().chain(queue_rows).collect();

	Ok(records)
}

fn store(db: &Database, records: &Records) {
	for (column, key, value) in records {
		let column: &str = column;

		db[column].insert(key, value);
	}
}

fn erase(db: &Database, records: &Records) {
	for (column, key, _) in records {
		let column: &str = column;

		db[column].remove(key);
	}
}

fn packed(shortroomid: u64, count: PduCount) -> RawPduId { PduId { shortroomid, count }.into() }

fn event(id: &EventId, room: &RoomId) -> Value {
	json!({
		"event_id": id, "room_id": room, "sender": "@local:localhost",
		"type": "m.room.message", "content": {"msgtype": "m.text", "body": "preserve"},
		"origin": "localhost", "origin_server_ts": 1, "depth": 1,
		"prev_events": [], "auth_events": [], "signatures": {},
		"hashes": {"sha256": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"},
	})
}

fn token(short: u64, id: &RawPduId) -> Vec<u8> {
	[short.to_be_bytes().as_slice(), b"preserve", &[SEP], id.as_bytes()].concat()
}

#[tracing::instrument(level = "trace", skip_all)]
async fn snapshot(db: &Database, columns: &[&'static str]) -> Result<Records> {
	columns
		.try_stream()
		.try_fold(Records::new(), async |records, column| {
			let name: &str = column;

			let rows: Records = db[name]
				.raw_stream()
				.map_ok(|(key, value)| (*column, key.to_vec(), value.to_vec()))
				.try_collect()
				.await?;

			Ok(combine(records, rows))
		})
		.await
}

fn combine(mut records: Records, rows: Records) -> Records {
	records.extend(rows);
	records
}

#[tracing::instrument(level = "debug", skip_all)]
async fn assert_records(db: &Database, records: &Records) -> Result {
	for (column, key, value) in records {
		let column: &str = column;

		assert_eq!(db[column].get(key).await?.as_ref(), value, "{column}");
	}

	Ok(())
}

#[tracing::instrument(level = "debug", skip_all)]
async fn assert_absent(db: &Database, column: &str, key: &[u8], message: &str) {
	assert!(db[column].get(key).await.is_missing(), "{message}");
}

fn relocated<'a>(
	source: &'a [Record],
	destination: &'a [Record],
) -> impl Iterator<Item = (&'a Record, &'a Record)> {
	source
		.iter()
		.zip(destination)
		.filter(|(old, new)| old.1 != new.1)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn create_state(services: &Services, room: &RoomId) -> Result {
	let create = event_id!("$create:localhost");
	let sender = user_id!("@local:localhost");
	let kind = StateEventType::RoomCreate;
	let pdu = json!({
		"event_id": create, "room_id": room, "sender": sender,
		"type": kind, "state_key": "", "content": {"creator": sender, "room_version": "10"},
		"origin": "localhost", "origin_server_ts": 1, "depth": 1,
		"prev_events": [], "auth_events": [], "signatures": {},
		"hashes": {"sha256": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"},
	});

	services.db["eventid_outlierpdu"].insert(create, to_vec(&pdu)?);
	let key = services
		.short
		.get_or_create_shortstatekey(&kind, "")
		.await;

	let entry = services
		.state_compressor
		.compress_state_event(key, create)
		.await;

	let state = Arc::new([entry].into());
	let shortstatehash = services
		.state
		.set_event_state(create, room, state)
		.await?;

	let lock = services.state.mutex.lock(room).await;

	services
		.state
		.set_room_state(room, shortstatehash, &lock);

	Ok(())
}
