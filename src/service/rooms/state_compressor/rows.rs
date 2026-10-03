//! Reads snapshot layers from record rows and repairs absent derived layers.
//!
//! Encoded key tails order records across snapshots, with the nearest layer
//! deciding each record's presence. Blobs supply missing layers atomically.

use std::iter::once;

use futures::{Stream, StreamExt, TryStreamExt, stream::try_unfold};
use ruma::events::StateEventType;
use serde_bytes::Bytes;
use tuwunel_core::{
	Err, Error, Result,
	arrayvec::ArrayVec,
	at, err, expected, implement, info,
	itertools::{EitherOrBoth, Itertools},
	smallvec::SmallVec,
	utils::{
		result::NotFound,
		stream::{IterStream, ReadyExt, TryBroadbandExt, TryReadyExt, WidebandExt},
		u64_from_bytes,
	},
};
use tuwunel_database::{
	Interfix, Map, Txn, deserialize_from_slice, keyval::KeyBuf, map, serialize_to,
};
use tuwunel_matrix::StateKey;

use super::{
	CompressedState, Service, StateDiff, compress_state_event, parse_compressed_state_event,
};
use crate::rooms::short::{ShortEventId, ShortStateHash, ShortStateKey};

/// Layer difference length followed by up to three nearest-first ancestors.
///
/// The fixed capacity matches the compressor's four-layer bound.
pub(crate) type Meta = ArrayVec<u64, 4>;

pub(super) type Resolved = SmallVec<[(KeyBuf, [u8; 9]); 2]>;
type Dictionary = (Vec<u8>, Vec<(ShortStateKey, usize)>);
type Ancestors = ArrayVec<ShortStateHash, 3>;
type Layers = ArrayVec<Vec<(Row, Tag, usize)>, 4>;

#[derive(Clone, Copy)]
enum Tag {
	Added = 1,
	Removed = 2,
	Both = 3,
}

/// One surviving record in encoded key order.
///
/// The owned tail outlives the database cursor; short IDs require no dictionary reads.
pub(crate) struct Row {
	/// Encoded event type, state key, and event ID without the hash prefix.
	pub(crate) tail: KeyBuf,

	/// Reverse-resolved short state key stored alongside the layer tag.
	pub(crate) shortstatekey: ShortStateKey,

	/// Short event ID decoded from the key suffix.
	pub(crate) shorteventid: ShortEventId,
}

/// Loads merged snapshot records, optionally restricted by type and state key.
///
/// Each layer is buffered before merging; the returned iterator resolves equal
/// tails lazily and suppresses records removed by their nearest layer.
/// A state key narrows the scan only together with an event type.
#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
pub(crate) async fn rows(
	&self,
	hash: ShortStateHash,
	event_type: Option<&StateEventType>,
	state_key: Option<&str>,
) -> Result<impl Iterator<Item = Row> + Send + use<>> {
	let meta = self.row_meta(hash).await?;
	let prefix: KeyBuf = match (event_type, state_key) {
		| (Some(kind), Some(key)) => serialize_to((kind.to_cow_str(), key, Interfix))?,
		| (Some(kind), None) => serialize_to((kind.to_cow_str(), Interfix))?,
		| _ => KeyBuf::new(),
	};

	let layers: Layers = once(hash)
		.chain(meta.into_iter().skip(1))
		.enumerate()
		.stream()
		.then(async |(depth, hash)| {
			let prefix: KeyBuf = serialize_to((hash, Bytes::new(prefix.as_slice())))?;

			self.services.db[map!("shortstatehash_statedelta")]
				.raw_stream_prefix(&prefix)
				.ready_and_then(|(key, value)| decode(key, value))
				.map_ok(move |(row, tag)| (row, tag, depth))
				.try_collect()
				.await
		})
		.try_collect()
		.await?;

	let rows = layers
		.into_iter()
		.kmerge_by(|(a, _, a_depth), (b, _, b_depth)| (&a.tail, a_depth) < (&b.tail, b_depth))
		.dedup_by(|a, b| a.0.tail == b.0.tail)
		.filter_map(|(row, tag, _)| matches!(tag, Tag::Added).then_some(row));

	Ok(rows)
}

/// Loads snapshot metadata, deriving and persisting rows and meta on a miss.
///
/// Missing layers are reconstructed from their blobs, parents first.
#[implement(Service)]
#[tracing::instrument(level = "trace", skip(self))]
pub(super) async fn row_meta(&self, hash: ShortStateHash) -> Result<Meta> {
	let db = &self.services.db;
	let statemeta = &db[map!("shortstatehash_statemeta")];

	if let Some(value) = statemeta
		.get(&hash.to_be_bytes())
		.await
		.optional()?
	{
		return unpack(&value);
	}

	let blob = db[map!("shortstatehash_statediff")]
		.get(&hash.to_be_bytes())
		.await
		.map_err(|error| {
			err!(Database("Failed to find StateDiff from short {hash:?}: {error}"))
		})?;

	let parent = parent(&blob)?;
	let (added, removed) = parts(&blob)?;
	let parent_meta = if parent == 0 {
		None
	} else {
		Some(Box::pin(self.row_meta(parent)).await?) // recursion cycle
	};

	let ancestors = parent_meta
		.into_iter()
		.flat_map(|meta| once(parent).chain(meta.into_iter().skip(1)));

	let diff_len = u64::try_from(added.len().saturating_add(removed.len()) / 16)?;
	let (meta, packed) = metadata(diff_len, ancestors)?;
	let txn = records(added, removed)
		.stream()
		.wide_filter_map(async |record| self.resolve_record(record).await)
		.ready_fold(db.txn(), |mut txn, (key, value)| {
			txn.put_raw(
				&db[map!("shortstatehash_statedelta")],
				(hash, Bytes::new(key.as_slice())),
				value,
			);

			txn
		})
		.await;

	finish(txn, statemeta, hash, &packed);
	Ok(meta)
}

#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
pub(super) async fn layer_diff(
	&self,
	hash: ShortStateHash,
) -> Result<(CompressedState, CompressedState)> {
	let prefix: KeyBuf = serialize_to((hash, Interfix))?;
	let diff = self.services.db[map!("shortstatehash_statedelta")]
		.raw_stream_prefix(&prefix)
		.ready_and_then(|(key, value)| decode(key, value))
		.map_ok(|(row, tag)| (compress_state_event(row.shortstatekey, row.shorteventid), tag))
		.ready_try_fold(
			(CompressedState::new(), CompressedState::new()),
			|(mut added, mut removed), (record, tag)| {
				if matches!(tag, Tag::Added | Tag::Both) {
					added.insert(record);
				}

				if matches!(tag, Tag::Removed | Tag::Both) {
					removed.insert(record);
				}

				Ok((added, removed))
			},
		)
		.await?;

	Ok(diff)
}

#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
pub(super) async fn resolve_rows(&self, diff: &StateDiff) -> Resolved {
	tagged(diff.added.iter(), diff.removed.iter())
		.stream()
		.wide_filter_map(async |record| self.resolve_record(record).await)
		.collect()
		.await
}

impl Row {
	/// Decodes the state key for a returned record.
	///
	/// Query and merge operations compare raw tails without decoding strings.
	pub(crate) fn state_key(&self) -> Result<StateKey> {
		let (_, key, _): (&str, &str, u64) = deserialize_from_slice(&self.tail)?;

		Ok(key.into())
	}

	/// Returns the record's short state key and short event ID.
	///
	/// Both IDs are decoded when the row is read.
	pub(crate) fn shortids(self) -> (ShortStateKey, ShortEventId) {
		(self.shortstatekey, self.shorteventid)
	}
}

/// Populates derived snapshot rows from authoritative blobs.
///
/// The reverse dictionary uses one concatenated byte buffer and two words per key.
/// Each snapshot's rows and metadata share a transaction; interrupted scans
/// leave the migration marker absent so the next boot rebuilds them.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn populate_rows(&self) -> Result {
	let db = &self.services.db;
	let server = &self.services.server;
	let cork = db.cork_and_sync();
	let dictionary: Dictionary = db[map!("shortstatekey_statekey")]
		.raw_stream()
		.ready_and_then(|(key, value)| {
			server.check_running()?;
			server.progress.advance();
			Ok(u64_from_bytes(key).ok().map(|key| (key, value)))
		})
		.ready_try_filter_map(Ok)
		.ready_try_fold(
			(Vec::new(), Vec::new()),
			|(mut bytes, mut offsets), (key, value)| -> Result<_> {
				bytes.extend_from_slice(value);
				offsets.push((key, bytes.len()));
				Ok((bytes, offsets))
			},
		)
		.await?;

	let (snapshots, skipped, damaged) = db[map!("shortstatehash_statediff")]
		.raw_stream()
		.map_ok(|(key, blob)| (u64_from_bytes(key), blob.to_vec()))
		.broad_and_then(async |(hash, blob)| {
			server.check_running()?;
			server.progress.advance();
			let result = self
				.populate_snapshot(hash, &blob, &dictionary)
				.await;

			server.check_running()?;
			Ok(match result {
				| Ok(skipped) => (1_usize, skipped, 0_usize),
				| Err(_) => (0, 0, 1),
			})
		})
		.ready_try_fold(
			(0_usize, 0_usize, 0_usize),
			|(snapshots, skipped, damaged), (count, orphans, failed)| -> Result<_> {
				Ok((
					snapshots.saturating_add(count),
					skipped.saturating_add(orphans),
					damaged.saturating_add(failed),
				))
			},
		)
		.await?;

	server.check_running()?;
	drop(cork);
	info!(snapshots, skipped, damaged, "Populated snapshot rows");
	Ok(())
}

#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn populate_snapshot(
	&self,
	hash: Result<ShortStateHash>,
	blob: &[u8],
	dictionary: &Dictionary,
) -> Result<usize> {
	let hash = hash?;
	let ancestors: Ancestors = self
		.blob_ancestors(parent(blob)?)
		.try_collect()
		.await?;

	let (added, removed) = parts(blob)?;
	let diff_len = u64::try_from(added.len().saturating_add(removed.len()) / 16)?;
	let (_, packed) = metadata(diff_len, ancestors)?;
	let (txn, skipped) = self.stage_rows(hash, dictionary, added, removed)?;

	finish(txn, &self.services.db[map!("shortstatehash_statemeta")], hash, &packed);
	Ok(skipped)
}

#[implement(Service)]
#[tracing::instrument(level = "trace", skip(self))]
fn blob_ancestors(
	&self,
	hash: ShortStateHash,
) -> impl Stream<Item = Result<ShortStateHash>> + Send + '_ {
	try_unfold(hash, async |hash| -> Result<_> {
		if hash == 0 {
			return Ok(None);
		}

		let blob = self.services.db[map!("shortstatehash_statediff")]
			.get(&hash.to_be_bytes())
			.await?;

		let next = parent(&blob)?;

		Ok(Some((hash, next)))
	})
	.take(Ancestors::new().capacity())
}

#[implement(Service)]
fn stage_rows(
	&self,
	hash: ShortStateHash,
	dictionary: &Dictionary,
	added: &[u8],
	removed: &[u8],
) -> Result<(Txn, usize)> {
	let db = &self.services.db;

	records(added, removed)
		.map(|(record, tag)| {
			self.services.server.check_running()?;
			let (shortstatekey, shorteventid) = parse_compressed_state_event(record);
			let resolved = dictionary
				.1
				.binary_search_by_key(&shortstatekey, |entry| entry.0)
				.ok()
				.map(|index| {
					let start = dictionary.1[..index].last().map_or(0, at!(1));
					let key = &dictionary.0[start..dictionary.1[index].1];

					((hash, Bytes::new(key), shorteventid), value(tag, shortstatekey))
				});

			let skipped = usize::from(resolved.is_none());

			Ok((resolved, skipped))
		})
		.try_fold((db.txn(), 0_usize), |(mut txn, skipped), entry: Result<_>| {
			let (resolved, orphans) = entry?;

			if let Some((key, value)) = resolved {
				txn.put_raw(&db[map!("shortstatehash_statedelta")], key, value);
			}

			Ok((txn, skipped.saturating_add(orphans)))
		})
}

/// Decodes metadata and rejects lengths outside the four-layer format.
///
/// Every accepted value contains one to four complete big-endian integers.
pub(crate) fn unpack(raw: &[u8]) -> Result<Meta> {
	if !(8..=32).contains(&raw.len()) || !raw.len().is_multiple_of(8) {
		return Err!(Database("Invalid snapshot metadata length"));
	}

	raw.as_chunks::<8>()
		.0
		.iter()
		.copied()
		.map(u64::from_be_bytes)
		.map(Ok)
		.collect()
}

fn parts(blob: &[u8]) -> Result<(&[u8], &[u8])> {
	let parent = parent(blob)?;
	// The parent prefix precedes added records, a zero sentinel, and removed records.
	let body = &blob[8..];
	let split = body
		.as_chunks::<16>()
		.0
		.iter()
		.position(|record| record[..8] == [0; 8]);

	let Some(split) = split else {
		return Ok((body, &[]));
	};

	let (added, removed) = body.split_at(expected!(split * 16));

	Ok((added, if parent == 0 { &[] } else { &removed[8..] }))
}

/// Packs layer metadata as consecutive big-endian integers.
///
/// The first integer is the layer difference length, followed by ancestors.
pub(crate) fn pack(meta: &[u64]) -> Result<KeyBuf> { serialize_to(meta) }

fn records<'a>(added: &'a [u8], removed: &'a [u8]) -> impl Iterator<Item = ([u8; 16], Tag)> + 'a {
	let sorted = |run: &'a [u8]| {
		run.as_chunks::<16>()
			.0
			.iter()
			.sorted_unstable()
			.dedup()
	};

	tagged(sorted(added), sorted(removed))
}

fn tagged<'a>(
	added: impl Iterator<Item = &'a [u8; 16]>,
	removed: impl Iterator<Item = &'a [u8; 16]>,
) -> impl Iterator<Item = ([u8; 16], Tag)> {
	added
		.merge_join_by(removed, Ord::cmp)
		.map(|entry| match entry {
			| EitherOrBoth::Left(record) => (*record, Tag::Added),
			| EitherOrBoth::Right(record) => (*record, Tag::Removed),
			| EitherOrBoth::Both(record, _) => (*record, Tag::Both),
		})
}

#[implement(Service)]
async fn resolve_record(&self, (record, tag): ([u8; 16], Tag)) -> Option<(KeyBuf, [u8; 9])> {
	let (shortstatekey, shorteventid) = parse_compressed_state_event(record);
	let (kind, key) = self
		.services
		.short
		.get_statekey_from_short(shortstatekey)
		.await
		.ok()?;

	let tail = serialize_to((kind.to_cow_str(), key.as_str(), shorteventid)).ok()?;

	Some((tail, value(tag, shortstatekey)))
}

fn parent(blob: &[u8]) -> Result<ShortStateHash> {
	blob.first_chunk::<8>()
		.copied()
		.map(u64::from_be_bytes)
		.ok_or_else(|| err!(Database("Invalid snapshot blob header")))
}

fn metadata(
	diff_len: u64,
	ancestors: impl IntoIterator<Item = ShortStateHash>,
) -> Result<(Meta, KeyBuf)> {
	let meta: Meta = once(diff_len).chain(ancestors).collect();
	let packed = pack(&meta)?;

	Ok((meta, packed))
}

fn decode(key: &[u8], value: &[u8]) -> Result<(Row, Tag)> {
	let invalid = || err!(Database("Invalid snapshot row"));
	let (tag, value) = value.split_first().ok_or_else(invalid)?;
	let tag = Tag::try_from(*tag)?;
	let shortstatekey = u64_from_bytes(value)?;
	let tail = KeyBuf::from_slice(key.get(9..).ok_or_else(invalid)?);
	let shorteventid = u64::from_be_bytes(*tail.last_chunk().ok_or_else(invalid)?);

	Ok((Row { tail, shortstatekey, shorteventid }, tag))
}

impl From<Tag> for u8 {
	fn from(tag: Tag) -> Self {
		match tag {
			| Tag::Added => 1,
			| Tag::Removed => 2,
			| Tag::Both => 3,
		}
	}
}

impl TryFrom<u8> for Tag {
	type Error = Error;

	fn try_from(value: u8) -> Result<Self> {
		match value {
			| 1 => Ok(Self::Added),
			| 2 => Ok(Self::Removed),
			| 3 => Ok(Self::Both),
			| 0 | 4..=u8::MAX => Err!(Database("Invalid snapshot row tag")),
		}
	}
}

fn value(tag: Tag, shortstatekey: ShortStateKey) -> [u8; 9] {
	let mut value = [u8::from(tag); 9];

	value[1..].copy_from_slice(&shortstatekey.to_be_bytes());
	value
}

fn finish(mut txn: Txn, map: &Map, hash: ShortStateHash, meta: &[u8]) {
	txn.insert_raw(map, hash.to_be_bytes(), meta);
	txn.execute();
}
