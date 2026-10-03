//! Encodes room state snapshots as compact parent-linked deltas.
//!
//! Each state entry combines a short state key with a short event ID in a
//! fixed-width record. Derived rows serve reads; persistent delta depth remains
//! bounded; authoritative blobs support older binaries.

pub(crate) mod rows;

#[cfg(test)]
mod tests;

use std::{collections::BTreeSet, fmt::Debug, iter::once, sync::Arc};

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use ruma::{EventId, RoomId};
use serde_bytes::Bytes;
use tracing::Level;
use tuwunel_core::{
	Result,
	arrayvec::ArrayVec,
	at, checked, debug_error, implement,
	itertools::{EitherOrBoth, Itertools},
	utils,
	utils::{OptionExt, result::ErrLog, stream::IterStream},
};
use tuwunel_database::{Map, Txn, map};

use self::rows::{Meta, Resolved, pack};
use crate::rooms::short::{ShortEventId, ShortId, ShortStateHash, ShortStateKey};

/// Persists compressed room state snapshots and their derived record rows.
///
/// New snapshots are stored as bounded delta chains and flattened when their
/// depth or relative size becomes inefficient. Blobs and derived rows share
/// one transaction when a new snapshot is allocated.
pub struct Service {
	db: Data,
	services: Arc<crate::services::OnceServices>,
}

struct Data {
	shortstatehash_statediff: Arc<Map>,
}

/// One state as a delta against a parent state.
///
/// `added` and `removed` are the compressed entries this state adds to and
/// removes from its parent chain's accumulation; a `None` parent makes
/// `added` the full state.
#[derive(Clone)]
pub(crate) struct StateDiff {
	/// Parent snapshot against which this delta is applied, if any.
	pub(crate) parent: Option<ShortStateHash>,

	/// Compressed entries added to the parent snapshot.
	pub(crate) added: Arc<CompressedState>,

	/// Compressed entries removed from the parent snapshot.
	pub(crate) removed: Arc<CompressedState>,
}

/// Resolved snapshot delta ready for atomic allocation and storage.
///
/// All dictionary and parent reads finish before the caller's synchronous
/// transaction callback receives this value.
pub struct Prepared {
	diff: StateDiff,
	meta: Meta,
	rows: Resolved,
}

/// Reports a saved snapshot and its change from the room's previous state.
///
/// An unchanged snapshot reuses its short hash and returns empty added and
/// removed sets.
#[derive(Clone, Default)]
pub struct HashSetCompressStateEvent {
	/// Short hash identifying the saved snapshot.
	pub shortstatehash: ShortStateHash,

	/// Entries present only in the saved snapshot.
	pub added: Arc<CompressedState>,

	/// Entries present only in the previous snapshot.
	pub removed: Arc<CompressedState>,
}

/// Ordered set of compressed state-key and event-ID pairs.
///
/// Ordering makes hashing, differences, and persistent serialization
/// deterministic for the same logical state.
pub type CompressedState = BTreeSet<CompressedStateEvent>;

type Difference = (Arc<CompressedState>, Arc<CompressedState>);

/// Fixed-width encoding of one short state key and short event ID.
///
/// The first eight big-endian bytes hold the state key and the remaining eight
/// hold the event ID.
pub type CompressedStateEvent = [u8; 2 * size_of::<ShortId>()];

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				shortstatehash_statediff: args.db["shortstatehash_statediff"].clone(),
			},
			services: args.services.clone(),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Compresses a stream of state-key and event-ID pairs.
///
/// Missing short event IDs are allocated as the returned stream is polled, and
/// each result packs both short IDs into the fixed-width representation.
#[implement(Service)]
pub fn compress_state_events<'a, I>(
	&'a self,
	state: I,
) -> impl Stream<Item = CompressedStateEvent> + Send + 'a
where
	I: Iterator<Item = (&'a ShortStateKey, &'a EventId)> + Clone + Debug + Send + 'a,
{
	let event_ids = state.clone().map(at!(1));

	let short_event_ids = self
		.services
		.short
		.multi_get_or_create_shorteventid(event_ids);

	state
		.stream()
		.map(at!(0))
		.zip(short_event_ids)
		.map(|(shortstatekey, shorteventid)| compress_state_event(*shortstatekey, shorteventid))
}

/// Compresses one state key and event ID into its fixed-width representation.
///
/// A short event ID is allocated first when the event has not been seen.
#[implement(Service)]
pub async fn compress_state_event(
	&self,
	shortstatekey: ShortStateKey,
	event_id: &EventId,
) -> CompressedStateEvent {
	let shorteventid = self
		.services
		.short
		.get_or_create_shorteventid(event_id)
		.await;

	compress_state_event(shortstatekey, shorteventid)
}

/// Resolves and folds a delta before entering an allocation transaction.
///
/// Parent deltas fold into the new delta when depth or relative size requires it;
/// `diff_to_sibling` is the expected sibling delta size (2 normally, 1_000_000
/// for a state nothing will build on). Root removals are discarded.
#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
pub async fn prepare_state_diff(
	&self,
	added: Arc<CompressedState>,
	removed: Arc<CompressedState>,
	diff_to_sibling: usize,
	parent: Option<ShortStateHash>,
) -> Result<Prepared> {
	let diffsum = delta_len(&added, &removed)?;
	let ancestors = if let Some(parent) = parent {
		let meta = self.row_meta(parent).await?;
		let parent_diff = usize::try_from(meta[0])?;

		if meta.len() > 3
			|| checked!(diffsum * diffsum)? >= checked!(2 * diff_to_sibling * parent_diff)?
		{
			let (parent_added, parent_removed) = self.layer_diff(parent).await?;
			let (added, removed) = fold_delta(parent_added, parent_removed, &added, &removed);

			// recursion cycle
			return Box::pin(self.prepare_state_diff(
				Arc::new(added),
				Arc::new(removed),
				diffsum,
				meta.get(1).copied(),
			))
			.await;
		}

		Some(meta)
	} else {
		None
	};

	let removed = parent.map(|_| removed).unwrap_or_default();
	let diff_len = u64::try_from(delta_len(&added, &removed)?)?;
	let ancestors = ancestors
		.into_iter()
		.flat_map(|meta| parent.into_iter().chain(meta.into_iter().skip(1)));

	let meta = once(diff_len).chain(ancestors).collect();
	let diff = StateDiff { parent, added, removed };
	let rows = self.resolve_rows(&diff).await;

	Ok(Prepared { diff, meta, rows })
}

fn delta_len(added: &CompressedState, removed: &CompressedState) -> Result<usize> {
	let added = added.len();
	let removed = removed.len();

	checked!(added + removed)
}

fn fold_delta(
	parent_added: CompressedState,
	parent_removed: CompressedState,
	added: &CompressedState,
	removed: &CompressedState,
) -> (CompressedState, CompressedState) {
	let (parent_added, parent_removed) = cancel(parent_added, parent_removed, removed);
	let (removed, added) = cancel(parent_removed, parent_added, added);

	(added, removed)
}

fn cancel(
	from: CompressedState,
	into: CompressedState,
	records: &CompressedState,
) -> (CompressedState, CompressedState) {
	records
		.iter()
		.fold((from, into), |(mut from, mut into), record| {
			if !from.remove(record) {
				into.insert(*record);
			}

			(from, into)
		})
}

/// Saves a complete compressed snapshot and reports its previous-state delta.
///
/// An existing content hash is reused; otherwise the short hash and delta row
/// are created together. Failure to reconstruct the previous chain is treated
/// as an absent parent, while an exactly unchanged snapshot returns empty sets.
#[implement(Service)]
#[tracing::instrument(skip(self, new_state_ids_compressed), level = "debug")]
pub async fn save_state(
	&self,
	room_id: &RoomId,
	new_state_ids_compressed: Arc<CompressedState>,
) -> Result<HashSetCompressStateEvent> {
	let previous_shortstatehash = self
		.services
		.state
		.get_room_shortstatehash(room_id)
		.await
		.ok();

	let state_hash = utils::calculate_hash(
		new_state_ids_compressed
			.iter()
			.map(|bytes| &bytes[..]),
	);

	let existing_shortstatehash = self
		.services
		.short
		.get_shortstatehash(&state_hash)
		.await
		.ok();

	if let Some(new_shortstatehash) = existing_shortstatehash
		.filter(|&new_shortstatehash| previous_shortstatehash.eq(&Some(new_shortstatehash)))
	{
		return Ok(HashSetCompressStateEvent {
			shortstatehash: new_shortstatehash,
			..Default::default()
		});
	}

	let difference = previous_shortstatehash
		.map_async(|parent| self.parent_difference(parent, &new_state_ids_compressed))
		.await
		.and_then(|result| result.log_err(Level::DEBUG).ok());

	let parent = difference.as_ref().and(previous_shortstatehash);
	let (added, removed) =
		difference.unwrap_or_else(|| (new_state_ids_compressed.clone(), Arc::default()));

	if let Some(shortstatehash) = existing_shortstatehash {
		return Ok(HashSetCompressStateEvent { shortstatehash, added, removed });
	}

	// every state change is 2 event changes on average
	let prepared = self
		.prepare_state_diff(added.clone(), removed.clone(), 2, parent)
		.await;

	let (prepared, added, removed) = match prepared {
		| Err(error) if parent.is_some() => {
			debug_error!(%error, "Failed to prepare parent state; saving root");
			let prepared = self
				.prepare_state_diff(new_state_ids_compressed.clone(), Arc::default(), 2, None)
				.await?;

			(prepared, new_state_ids_compressed, Arc::default())
		},
		| prepared => (prepared?, added, removed),
	};

	let (shortstatehash, _) = self
		.services
		.short
		.get_or_create_shortstatehash(&state_hash, |txn, hash| {
			self.save_state_from_diff(txn, hash, prepared)
		})
		.await?;

	Ok(HashSetCompressStateEvent { shortstatehash, added, removed })
}

/// Compares a sorted snapshot with the merged records of its parent.
///
/// Only the resulting local differences use sets; the parent's full state is
/// sorted once for the merge and is not retained as a materialized frame.
/// Returns `(added, removed)` relative to the parent.
#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) async fn parent_difference(
	&self,
	parent: ShortStateHash,
	state: &CompressedState,
) -> Result<Difference> {
	let parent = self
		.rows(parent, None, None)
		.await?
		.map(|row| compress_state_event(row.shortstatekey, row.shorteventid))
		.sorted_unstable()
		.dedup();

	let fold = |(mut added, mut removed): (CompressedState, CompressedState), record| {
		match record {
			| EitherOrBoth::Both(..) => {},
			| EitherOrBoth::Left(record) => {
				added.insert(record);
			},
			| EitherOrBoth::Right(record) => {
				removed.insert(record);
			},
		}

		(added, removed)
	};

	let (added, removed) = state
		.iter()
		.copied()
		.merge_join_by(parent, Ord::cmp)
		.fold((CompressedState::new(), CompressedState::new()), fold);

	Ok((Arc::new(added), Arc::new(removed)))
}

/// Stages the authoritative blob and resolved rows in one transaction.
///
/// Preparation performs all asynchronous reads before this synchronous call;
/// the caller executes the transaction together with short-hash allocation.
#[implement(Service)]
pub fn save_state_from_diff(
	&self,
	txn: &mut Txn,
	hash: ShortStateHash,
	prepared: Prepared,
) -> Result {
	self.save_statediff(txn, hash, &prepared.diff);
	txn.put_raw(&self.services.db[map!("shortstatehash_statemeta")], hash, pack(&prepared.meta)?);

	for (tail, value) in prepared.rows {
		txn.put_raw(
			&self.services.db[map!("shortstatehash_statedelta")],
			(hash, Bytes::new(tail.as_slice())),
			value,
		);
	}

	Ok(())
}

/// Serializes one state's delta into the caller's transaction.
///
/// Added and removed entries are emitted in sorted set order. The removed run
/// follows an all-zero sentinel only when it is nonempty, and this method does
/// not execute the transaction.
#[implement(Service)]
pub(crate) fn save_statediff(
	&self,
	txn: &mut Txn,
	shortstatehash: ShortStateHash,
	diff: &StateDiff,
) {
	let event_count = diff
		.added
		.len()
		.saturating_add(diff.removed.len());

	let event_bytes = event_count.saturating_mul(size_of::<CompressedStateEvent>());
	let separator_bytes =
		usize::from(!diff.removed.is_empty()).saturating_mul(size_of::<ShortStateHash>());

	let capacity = size_of::<ShortStateHash>()
		.saturating_add(event_bytes)
		.saturating_add(separator_bytes);

	let parent = diff.parent.unwrap_or(0_u64);
	let mut value = Vec::<u8>::with_capacity(capacity);
	value.extend_from_slice(&parent.to_be_bytes());

	for new in diff.added.iter() {
		value.extend_from_slice(&new[..]);
	}

	if !diff.removed.is_empty() {
		value.extend_from_slice(&0_u64.to_be_bytes());
		for removed in diff.removed.iter() {
			value.extend_from_slice(&removed[..]);
		}
	}

	txn.insert_raw(&self.db.shortstatehash_statediff, shortstatehash.to_be_bytes(), value);
}

/// Packs a short state key and short event ID into one compressed record.
///
/// Both IDs use big-endian encoding so byte ordering follows numeric ordering.
#[inline]
#[must_use]
pub(crate) fn compress_state_event(
	shortstatekey: ShortStateKey,
	shorteventid: ShortEventId,
) -> CompressedStateEvent {
	const SIZE: usize = size_of::<CompressedStateEvent>();

	let mut v = ArrayVec::<u8, SIZE>::new();
	v.extend(shortstatekey.to_be_bytes());
	v.extend(shorteventid.to_be_bytes());
	v.as_ref()
		.try_into()
		.expect("failed to create CompressedStateEvent")
}

/// Unpacks a compressed state record into its two short IDs.
///
/// This is the inverse of [`compress_state_event`].
#[inline]
#[must_use]
pub(crate) fn parse_compressed_state_event(
	compressed_event: CompressedStateEvent,
) -> (ShortStateKey, ShortEventId) {
	use utils::u64_from_u8;

	let shortstatekey = u64_from_u8(&compressed_event[0..size_of::<ShortStateKey>()]);
	let shorteventid = u64_from_u8(&compressed_event[size_of::<ShortStateKey>()..]);

	(shortstatekey, shorteventid)
}
