use futures::{TryStreamExt, future::ready};
use tuwunel_core::{
	Err, Result, err,
	utils::{IterStream, TryReadyExt, stream::TryWidebandExt},
};
use tuwunel_database::{Database, Txn};

use self::Operation::{Publish, Restore};
use super::{
	Action, Row, lookup,
	rows::{COLUMNS, decode, rewrite},
};
use crate::Services;

type Writes = Vec<Write>;

type Changes = [Option<Write>; 2];

pub(super) enum Applied {
	Done,
	Refused,
	Restored,
}

#[derive(Clone, Copy)]
enum Operation {
	Publish,
	Restore,
}

struct Write {
	column: usize,
	key: Box<[u8]>,
	before: Option<Box<[u8]>>,
	after: Option<Box<[u8]>>,
}

#[tracing::instrument(level = "trace", skip_all)]
pub(super) async fn apply<I>(
	services: &Services,
	rows: I,
	from: u64,
	action: Action,
) -> Result<Applied>
where
	I: IntoIterator<Item = Row> + Send,
	I::IntoIter: Send,
{
	services.server.check_running()?;
	let db = &services.db;
	let writes = match action {
		| Action::Purge => rows.into_iter().map(removal).collect(),
		| Action::Move(to) => {
			let Some(writes) = preflight(db, rows, from, to).await? else {
				return Ok(Applied::Refused);
			};

			writes
		},
	};

	if commit(db, &writes, Publish).await? {
		return Ok(Applied::Done);
	}

	// Restoring after a durable delete is a no-op or overwrites a later writer's row.
	if matches!(action, Action::Purge) {
		return Err!("room deletion verification failed");
	}

	if !commit(db, &writes, Restore).await? {
		return Err!("room restoration verification failed");
	}

	Ok(Applied::Restored)
}

#[tracing::instrument(level = "trace", skip_all)]
async fn preflight<I>(db: &Database, rows: I, from: u64, to: u64) -> Result<Option<Writes>>
where
	I: IntoIterator<Item = Row> + Send,
	I::IntoIter: Send,
{
	// A refused row surfaces as Err(None), which stops the fold without an error.
	rows.try_stream()
		.wide_and_then(|row| prepared(db, row, from, to))
		.map_err(Some)
		.ready_try_fold(Writes::new(), |writes, changes| {
			changes
				.map(|changes| appended(writes, changes))
				.ok_or(None)
		})
		.await
		.map(Some)
		.or_else(|error| error.map_or(Ok(None), Err))
}

async fn prepared(db: &Database, row: Row, from: u64, to: u64) -> Result<Option<Changes>> {
	let Some((key, value)) = decode(row.column, &row.key, &row.value)
		.and_then(|decoded| rewrite(decoded, row.column, &row.key, &row.value, from, to))
	else {
		return Ok(None);
	};

	let moved = key != row.key;
	let before = lookup(db, COLUMNS[row.column], &key, |previous| match previous {
		| None => moved.then_some(None),
		| Some(previous) if moved => previous
			.eq(&*value)
			.then(|| Some(Box::from(previous))),
		| Some(previous) => previous.eq(&*row.value).then_some(None),
	})
	.await?;

	Ok(before.map(|before| changes(row, key, value, before)))
}

fn changes(row: Row, key: Box<[u8]>, value: Box<[u8]>, before: Option<Box<[u8]>>) -> Changes {
	let source = removal(row);

	if source.key == key {
		return [Some(Write { after: Some(value), ..source }), None];
	}

	let destination = Write {
		column: source.column,
		key,
		before,
		after: Some(value),
	};

	[Some(source), Some(destination)]
}

fn removal(row: Row) -> Write {
	Write {
		column: row.column,
		key: row.key,
		before: Some(row.value),
		after: None,
	}
}

fn appended(mut writes: Writes, changes: Changes) -> Writes {
	writes.extend(changes.into_iter().flatten());

	writes
}

async fn commit(db: &Database, writes: &[Write], operation: Operation) -> Result<bool> {
	publication(db, writes, operation)
		.try_execute()
		.map_err(|error| err!("{error}"))?;

	verified(db, writes, operation).await
}

fn publication(db: &Database, writes: &[Write], operation: Operation) -> Txn {
	writes
		.iter()
		.map(|write| (&db[COLUMNS[write.column]], &write.key, expected(write, operation)))
		.fold(db.txn(), |mut txn, (map, key, value)| {
			match value {
				| None => txn.del_raw(map, key),
				| Some(value) => txn.insert_raw(map, key, value),
			}

			txn
		})
}

#[tracing::instrument(level = "trace", skip_all)]
async fn verified(db: &Database, writes: &[Write], operation: Operation) -> Result<bool> {
	writes
		.iter()
		.try_stream()
		.wide_and_then(|write| {
			lookup(db, COLUMNS[write.column], &write.key, move |value| {
				value == expected(write, operation)
			})
		})
		.try_all(ready)
		.await
}

fn expected(write: &Write, operation: Operation) -> Option<&[u8]> {
	match operation {
		| Publish => write.after.as_deref(),
		| Restore => write.before.as_deref(),
	}
}
