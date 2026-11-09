//! Read-only integrity checks over the database.
//!
//! A check walks the columns it cares about and yields a finding for every
//! defect it sees, writing nothing. A finding describes the database as it
//! stands, so a check run after a repair sees that repair's work. The migrations
//! are a caller like any other: they consume these streams and write what a
//! finding calls for, re-deciding anything a write may have changed.

pub mod account_status;
pub mod columns;
pub mod email_bindings;
pub mod forbidden_names;
pub mod joined_members;
pub mod media_files;
pub mod mediaid_user;
pub mod moderation;
pub mod pdu_room_ids;
pub mod readreceiptid_readreceipt;
pub mod redactions;
pub mod referencedevents;
pub mod relatesto_typed;
pub mod roomid_tscount_pducount;
pub mod roomuserid_joined;
mod scan;
pub mod schema;
pub(crate) mod short_ids;
#[cfg(test)]
mod tests;
pub mod thread_summaries;
pub mod threadrootid_latestcount;
pub mod token_expiry;
pub mod userid_password;
pub mod useridprofilekey_value;
pub mod userroomid_highlightcount;

use std::{collections::BTreeMap, fmt::Display};

use futures::{FutureExt, Stream, StreamExt, TryFutureExt, future::BoxFuture, stream::unfold};
use ruma::{OwnedRoomId, RoomId, api::error::ErrorKind};
use serde::Deserialize;
use tuwunel_core::{Err, Error, Result, implement, utils::TryReadyExt};
use tuwunel_database::SEP;

pub(crate) use self::scan::ScanExt;
use crate::Services;

type Walk = for<'a> fn(&'a Services, usize) -> BoxFuture<'a, Result<Summary>>;

// The room `per_room` last answered for, and its answer.
type Answered<V> = Option<(OwnedRoomId, V)>;

// A row as `per_room` passes it: the outer result is the read, the inner one
// whether the row names a room.
type Row<A, B> = Result<Result<A, B>>;

/// Every check, in the order a whole run takes them.
static CHECKS: &[&Check] = &[
	&schema::CHECK,
	&columns::CHECK,
	&columns::UNUSED,
	&pdu_room_ids::CHECK,
	&roomuserid_joined::CHECK,
	&referencedevents::CHECK,
	&readreceiptid_readreceipt::CHECK,
	&userroomid_highlightcount::CHECK,
	&mediaid_user::LEGACY,
	&mediaid_user::REMOTE,
	&media_files::LEGACY,
	&media_files::CHECK,
	&useridprofilekey_value::CHECK,
	&userid_password::CHECK,
	&forbidden_names::USERNAMES,
	&forbidden_names::ALIASES,
	&joined_members::CHECK,
	&moderation::CHECK,
	&account_status::CHECK,
	&email_bindings::CHECK,
	&token_expiry::CHECK,
	&roomid_tscount_pducount::CHECK,
	&relatesto_typed::CHECK,
	&redactions::CHECK,
	&thread_summaries::CHECK,
	&threadrootid_latestcount::CHECK,
	&short_ids::CHECK,
];

/// A finding of any check.
///
/// Rendering it describes the one defect, while its kind names the class of
/// defect, the unit a run counts by.
pub trait Report: Display + Send {
	/// Names the class of defect.
	///
	/// A run counts its findings under this label, so every finding of one class
	/// answers with the same text.
	fn kind(&self) -> &'static str;
}

/// One registered check.
///
/// The registry is how a caller reaches a check by name, as the admin suite does.
/// A caller that wants the findings themselves takes the typed stream of the
/// check's own module.
#[derive(Clone, Copy)]
pub struct Check {
	/// The name a caller selects it by.
	pub name: &'static str,

	/// One line saying what it verifies.
	pub about: &'static str,

	/// Whether it needs a database nothing else is writing.
	///
	/// A check comparing columns reads them at different moments, so on a serving
	/// database a concurrent write would read as a defect.
	pub quiescent: bool,

	walk: Walk,
}

/// What one run of a check found.
///
/// Counting holds no finding in memory, so a run over a badly damaged database
/// stays bounded by its sample limit.
#[derive(Default)]
pub struct Summary {
	/// Findings of each kind.
	pub counts: BTreeMap<&'static str, u64>,

	/// The first findings rendered, up to the limit of the run.
	pub samples: Vec<String>,
}

/// Finds the check registered under a name.
///
/// Names are unique, and `None` means no check has this one.
#[must_use]
pub fn find(name: &str) -> Option<&'static Check> { checks().find(|check| check.name == name) }

/// Iterates over every registered check.
///
/// The order is the one a whole run takes, cheapest and most fundamental first.
pub fn checks() -> impl Iterator<Item = &'static Check> + Clone + Send { CHECKS.iter().copied() }

/// Runs this check to the end, keeping the first `limit` findings rendered.
///
/// A quiescent check refuses a writable database, because everything it would
/// report could be a concurrent write. The first error ends the run, whatever it
/// found by then.
#[implement(Check)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		check = %self.name,
	),
)]
pub async fn run(&self, services: &Services, limit: usize) -> Result<Summary> {
	if !self.runnable(services) {
		return Err!("{} needs a database opened read-only", self.name);
	}

	(self.walk)(services, limit).await
}

/// Whether this check can give a true answer on this database.
///
/// Only a quiescent check on a writable database cannot.
#[implement(Check)]
#[must_use]
pub fn runnable(&self, services: &Services) -> bool {
	!self.quiescent || services.db.is_read_only()
}

/// The number of findings of every kind.
///
/// This is zero exactly when the check found the database clean.
#[implement(Summary)]
#[must_use]
pub fn findings(&self) -> u64 {
	self.counts
		.values()
		.copied()
		.fold(0, u64::saturating_add)
}

fn tally<'a, S, F>(findings: S, limit: usize) -> BoxFuture<'a, Result<Summary>>
where
	S: Stream<Item = Result<F>> + Send + 'a,
	F: Report + 'a,
{
	findings
		.ready_try_fold(Summary::default(), move |summary, finding| {
			Ok(summary.record(&finding, limit))
		})
		.boxed() // type-erasure boundary
}

#[implement(Summary)]
fn record(mut self, finding: &impl Report, limit: usize) -> Self {
	if self.samples.len() < limit {
		self.samples.push(finding.to_string());
	}

	let count = self.counts.entry(finding.kind()).or_default();

	*count = count.saturating_add(1);
	self
}

/// Splits off the leading record of a stored key or value, as a release build
/// decodes a lone string from it.
///
/// The rest is whatever follows the separator ending that record, or `None`
/// when no separator does. A release build ignores the rest, where a debug build
/// asserts there is none.
pub(crate) fn leading_record(bytes: &[u8]) -> (&[u8], Option<&[u8]>) {
	let split = bytes
		.iter()
		.position(|&byte| byte == SEP)
		.and_then(|at| bytes.split_at_checked(at));

	split.map_or((bytes, None), |(leading, rest)| (leading, rest.get(1..)))
}

/// Decodes the JSON object a stored row holds, as a release build's database
/// decoder reads it.
///
/// That decoder parses the object at the start of the leading record and, in
/// either build, ignores whatever follows the object within that record. A
/// release build also ignores any further record, where a debug build panics on
/// one beyond a single trailing separator.
pub(crate) fn leading_json<T>(bytes: &[u8]) -> Result<T>
where
	T: for<'de> Deserialize<'de>,
{
	let (record, _) = leading_record(bytes);

	T::deserialize(&mut serde_json::Deserializer::from_slice(record)).map_err(Into::into)
}

/// Answers `damage` for a failed read that found its data but could not decode
/// it, and keeps any other failure.
///
/// A check reports such a row as damage and moves on, while a storage fault still
/// ends it.
pub(crate) fn undecodable_as<T>(error: Error, damage: T) -> Result<T> {
	undecodable(&error).then_some(damage).ok_or(error)
}

/// Whether a failed read found its data but could not decode it.
///
/// A row that is not JSON, or content that does not fit, fails this way, and a
/// storage fault never does.
pub(crate) fn undecodable(error: &Error) -> bool {
	matches!(error.kind(), ErrorKind::BadJson | ErrorKind::NotJson)
}

/// Pairs each row naming a room with what `resolve` answers for that room,
/// asking once for each run of consecutive rows in one room.
///
/// A walk sorted by room asks once per room and holds only the latest answer,
/// while a row naming no room passes through. A failed answer is yielded in
/// place of the row that asked.
fn per_room<'a, S, A, B, V, F, Fut>(
	rows: S,
	room: fn(&A) -> &RoomId,
	resolve: F,
) -> impl Stream<Item = Row<(A, V), B>> + Send + 'a
where
	S: Stream<Item = Row<A, B>> + Send + 'a,
	A: Send + 'a,
	B: Send + 'a,
	V: Clone + Send + 'a,
	F: Fn(OwnedRoomId) -> Fut + Send + Sync + 'a,
	Fut: Future<Output = Result<V>> + Send + 'a,
{
	let rows = Box::pin(rows); // Unpin for StreamExt::next

	unfold((rows, None, room, resolve), async |(mut rows, answered, room, resolve)| {
		let row = rows.next().await?;
		let (row, answered) = answer(row, answered, room, &resolve).await;

		Some((row, (rows, answered, room, resolve)))
	})
}

async fn answer<A, B, V, F, Fut>(
	row: Row<A, B>,
	answered: Answered<V>,
	room: fn(&A) -> &RoomId,
	resolve: &F,
) -> (Row<(A, V), B>, Answered<V>)
where
	A: Send,
	B: Send,
	V: Clone + Send,
	F: Fn(OwnedRoomId) -> Fut + Sync,
	Fut: Future<Output = Result<V>> + Send,
{
	let row = match row {
		| Ok(Ok(row)) => row,
		| Ok(Err(other)) => return (Ok(Err(other)), answered),
		| Err(error) => return (Err(error), answered),
	};

	let answered = match answered {
		| Some((room_id, answer)) if &*room_id == room(&row) => Ok((room_id, answer)),
		| _ => {
			let room_id = room(&row).to_owned();

			resolve(room_id.clone())
				.map_ok(|answer| (room_id, answer))
				.await
		},
	};

	match answered {
		| Ok((room_id, answer)) => (Ok(Ok((row, answer.clone()))), Some((room_id, answer))),
		| Err(error) => (Err(error), None),
	}
}
