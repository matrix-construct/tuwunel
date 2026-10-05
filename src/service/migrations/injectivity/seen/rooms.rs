use std::{
	collections::{BTreeMap, BTreeSet},
	str::from_utf8,
};

use futures::{
	Stream, StreamExt, TryStreamExt,
	future::{ready, try_join},
};
use publication::{Applied, apply};
use rows::{COLUMNS, SCANNED, decode, hints, word};
use ruma::RoomId;
use serde_json::from_slice;
use tuwunel_core::{
	PduEvent, Result, err,
	utils::{BoolExt, IterStream, TryReadyExt, result::NotFound, stream::TryWidebandExt},
};
use tuwunel_database::{Database, Interfix};

use super::sweep;
use crate::Services;

mod publication;
mod rows;
#[cfg(test)]
mod tests;

type Keys = BTreeMap<u64, Vec<Indexed>>;

type Indexed = (usize, Box<[u8]>);

pub(super) struct Rooms {
	pub(super) unfinished: BTreeSet<u64>,
	pub(super) unknown: bool,
	pub(super) deleted: u64,
	pub(super) moved: u64,
	pub(super) restored: bool,
}

#[derive(Default)]
struct Census {
	candidates: BTreeSet<u64>,
	held: BTreeSet<u64>,
	claims: BTreeMap<u64, usize>,
	keys: Keys,
	owner_unknown: bool,
	residue_unknown: bool,
}

struct Row {
	column: usize,
	key: Box<[u8]>,
	value: Box<[u8]>,
}

#[derive(Clone, Copy)]
enum Action {
	Purge,
	Move(u64),
}

#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn inspect(services: &Services) -> Result<Rooms> {
	Ok(pending(&census(services).await?))
}

#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn repair(services: &Services) -> Result<Rooms> {
	let census = census(services).await?;
	let result = pending(&census);

	if census.owner_unknown {
		return Ok(result);
	}

	let (result, restored) = census
		.candidates
		.difference(&census.held)
		.copied()
		.try_stream()
		.try_fold((result, false), async |(result, restored), candidate| {
			services.server.check_running()?;
			let keys = census
				.keys
				.get(&candidate)
				.map(Vec::as_slice)
				.unwrap_or_default();

			let rows = owned(services, candidate, keys).await?;

			let Some(action) = classify(services, &census, candidate, &rows).await? else {
				return Ok((result, restored));
			};

			match apply(services, rows, candidate, action).await? {
				| Applied::Done => Ok((completed(result, candidate, action), restored)),
				| Applied::Refused => Ok((result, restored)),
				| Applied::Restored => Ok((result, true)),
			}
		})
		.await?;

	Ok(Rooms { restored, ..result })
}

#[tracing::instrument(level = "debug", skip_all)]
async fn census(services: &Services) -> Result<Census> {
	let census = sweep(services, "roomid_shortroomid", Census::default(), owner).await?;

	COLUMNS
		.into_iter()
		.enumerate()
		.try_stream()
		.try_fold(census, async |census, (column, name)| {
			sweep(services, name, census, |census, key, value| {
				residue(census, column, key, value)
			})
			.await
		})
		.await
}

fn owner(census: Census, key: &[u8], value: &[u8]) -> Census {
	let id = value
		.get(..8)
		.and_then(word)
		.filter(|id| *id != 0);

	let valid =
		from_utf8(key).is_ok_and(|key| <&RoomId>::try_from(key).is_ok()) && word(value).is_some();

	claimed(census, id, valid)
}

fn claimed(mut census: Census, id: Option<u64>, valid: bool) -> Census {
	match id {
		| None => census.owner_unknown = true,
		| Some(id) => {
			let claims = census.claims.entry(id).or_default();

			*claims = claims.saturating_add(1);
			if !valid {
				census.held.insert(id);
			}
		},
	}

	census
}

fn residue(census: Census, column: usize, key: &[u8], value: &[u8]) -> Census {
	let decoded = decode(column, key, value);
	let ids = decoded
		.map(|row| row.rooms.map(|id| id.ne(&0).then_some(id)))
		.unwrap_or_else(|| hints(column, key, value));

	index(census, column, key, ids, decoded.is_some())
}

fn index(
	mut census: Census,
	column: usize,
	key: &[u8],
	ids: [Option<u64>; 2],
	valid: bool,
) -> Census {
	census.residue_unknown |= !valid && ids == [None; 2];
	ids.into_iter()
		.enumerate()
		.filter_map(|(position, id)| id.filter(|id| position == 0 || Some(*id) != ids[0]))
		.for_each(|id| {
			if !valid || ids.into_iter().flatten().any(|other| other != id) {
				census.held.insert(id);
			}

			if !census.claims.contains_key(&id) {
				census.candidates.insert(id);
				indexed(&mut census.keys, id, column, key);
			}
		});

	census
}

fn indexed(keys: &mut Keys, id: u64, column: usize, key: &[u8]) {
	if !SCANNED.contains(&column) {
		keys.entry(id)
			.or_default()
			.push((column, key.into()));
	}
}

fn pending(census: &Census) -> Rooms {
	let unfinished = census
		.candidates
		.union(&census.held)
		.copied()
		.collect();

	Rooms {
		unfinished,
		unknown: census.owner_unknown || census.residue_unknown,
		deleted: 0,
		moved: 0,
		restored: false,
	}
}

#[tracing::instrument(level = "trace", skip_all)]
async fn owned(services: &Services, candidate: u64, keys: &[Indexed]) -> Result<Vec<Row>> {
	let prefix = candidate.to_be_bytes();
	let scanned = SCANNED
		.stream()
		.map(|column| scan(services, column, &prefix))
		.flatten();

	let indexed = keys
		.iter()
		.try_stream()
		.wide_and_then(async |(column, key)| {
			services.server.check_running()?;
			let value = lookup(&services.db, COLUMNS[*column], key, |value| value.map(Box::from))
				.await?
				.ok_or_else(|| err!("room residue changed during census"))?;

			Ok(Row { column: *column, key: key.clone(), value })
		});

	scanned.chain(indexed).try_collect().await
}

fn scan<'a>(
	services: &'a Services,
	column: usize,
	prefix: &'a [u8],
) -> impl Stream<Item = Result<Row>> + Send + 'a {
	services.db[COLUMNS[column]]
		.raw_stream_prefix(prefix)
		.ready_and_then(move |(key, value)| {
			services.server.check_running()?;
			Ok(Row {
				column,
				key: key.into(),
				value: value.into(),
			})
		})
}

#[tracing::instrument(level = "trace", skip_all)]
async fn lookup<T>(
	db: &Database,
	column: &str,
	key: &[u8],
	project: impl FnOnce(Option<&[u8]>) -> T + Send,
) -> Result<T> {
	db[column]
		.get(key)
		.await
		.present()
		.map(|value| project(value.as_deref()))
}

#[tracing::instrument(level = "trace", skip_all)]
async fn classify(
	services: &Services,
	census: &Census,
	candidate: u64,
	rows: &[Row],
) -> Result<Option<Action>> {
	if rows.iter().any(|row| !unmixed(row, candidate)) || dangling(rows) {
		return Ok(None);
	}

	let Some(pdu) = rows
		.iter()
		.find(|row| row.column == 0)
		.and_then(pdu)
	else {
		return Ok(None);
	};

	let room = &pdu.room_id;
	let invalid = rows
		.iter()
		.filter(|row| row.column == 0)
		.try_stream()
		.wide_and_then(|row| misindexed(services, room, row))
		.try_any(ready)
		.await?;

	if invalid {
		return Ok(None);
	}

	let forward =
		lookup(&services.db, "roomid_shortroomid", room.as_bytes(), |forward| forward.map(word));

	let pointer = lookup(&services.db, "roomid_shortstatehash", room.as_bytes(), |pointer| {
		pointer.is_some()
	});

	let (forward, pointer) = try_join(forward, pointer).await?;

	let movable = |id: &u64| {
		*id != 0
			&& *id != candidate
			&& census.claims.get(id) == Some(&1)
			&& !census.held.contains(id)
	};

	let action = match (forward, pointer) {
		| (None, false) if !census.residue_unknown => has_leaves(services, room)
			.await?
			.is_false()
			.then_some(Action::Purge),
		| (Some(forward), true) => has_leaves(services, room)
			.await?
			.into_option()
			.and_then(|()| forward.filter(movable))
			.map(Action::Move),
		| _ => None,
	};

	Ok(action)
}

fn unmixed(row: &Row, candidate: u64) -> bool {
	decode(row.column, &row.key, &row.value).is_some_and(|row| {
		row.rooms
			.into_iter()
			.all(|id| id == 0 || id == candidate)
	})
}

fn dangling(rows: &[Row]) -> bool {
	let pdus: BTreeSet<&[u8]> = rows
		.iter()
		.filter(|row| row.column == 0)
		.map(|row| &*row.key)
		.collect();

	rows.iter()
		.filter(|row| row.column == 1)
		.any(|row| !pdus.contains(&*row.value))
}

fn pdu(row: &Row) -> Option<PduEvent> { from_slice(&row.value).ok() }

async fn misindexed(services: &Services, room: &RoomId, row: &Row) -> Result<bool> {
	let Some(pdu) = pdu(row).filter(|pdu| pdu.room_id == room) else {
		return Ok(true);
	};

	lookup(&services.db, COLUMNS[1], pdu.event_id.as_bytes(), |mapping| {
		mapping != Some(&*row.key)
	})
	.await
}

async fn has_leaves(services: &Services, room: &RoomId) -> Result<bool> {
	services.db["roomid_pduleaves"]
		.keys_prefix_raw(&(room, Interfix))
		.take(1)
		.ready_try_fold(false, |_, _| Ok(true))
		.await
}

fn completed(mut result: Rooms, candidate: u64, action: Action) -> Rooms {
	result.unfinished.remove(&candidate);
	match action {
		| Action::Purge => result.deleted = result.deleted.saturating_add(1),
		| Action::Move(_) => result.moved = result.moved.saturating_add(1),
	}

	result
}
