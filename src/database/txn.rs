//! Atomic database writes backed by one RocksDB write batch.
//!
//! A transaction queues operations for maps owned by one database engine and
//! commits them only when [`Txn::execute`] consumes it. Typed operations use
//! the database codec, while raw operations preserve caller-provided bytes.

use std::{fmt::Debug, iter::once, sync::Arc};

use rocksdb::WriteBatch;
use serde::Serialize;
use tuwunel_core::implement;

use crate::{
	Engine, Map,
	keyval::{serialize_key, serialize_val},
	util::or_else,
};

/// Atomic write batch spanning one or more column families from one database.
///
/// Every queued map must belong to the captured engine because column family
/// identifiers are interpreted within that database. Dropping an unexecuted
/// transaction leaves the database unchanged.
#[must_use = "does nothing until execute()"]
pub struct Txn {
	batch: WriteBatch,
	engine: Arc<Engine>,
}

/// Record parser yielding each queued key with its resolved map.
struct Keys<'a> {
	engine: &'a Engine,
	data: &'a [u8],
}

/// Batch representation header: a fixed64 sequence then a fixed32 count.
const HEADER: usize = 12;

/// Worst-case per-record overhead: a type tag and three varint32s.
const PER_OP: usize = 16;

/// Record tags per rocksdb `write_batch.cc`; puts and deletes against
/// column family id 0 encode as the legacy untagged types.
#[derive(Clone, Copy)]
enum Tag {
	Deletion = 0x0,
	Value = 0x1,
	CfDeletion = 0x4,
	CfValue = 0x5,
}

impl TryFrom<u8> for Tag {
	type Error = u8;

	fn try_from(byte: u8) -> Result<Self, Self::Error> {
		match byte {
			| 0x0 => Ok(Self::Deletion),
			| 0x1 => Ok(Self::Value),
			| 0x4 => Ok(Self::CfDeletion),
			| 0x5 => Ok(Self::CfValue),
			| unrecognized => Err(unrecognized),
		}
	}
}

/// Creates an empty transaction for one database engine.
///
/// Operations can be appended through the typed or raw queueing methods. The
/// transaction remains inert until [`Txn::execute`] consumes it.
#[implement(Txn)]
pub fn new(engine: &Arc<Engine>) -> Self {
	Self {
		batch: WriteBatch::default(),
		engine: engine.clone(),
	}
}

/// Creates an empty transaction with reserved batch capacity.
///
/// `capacity_bytes` reserves storage for the serialized RocksDB batch
/// representation. The reservation affects allocation only and does not queue
/// an operation.
#[implement(Txn)]
pub fn with_capacity_bytes(engine: &Arc<Engine>, capacity_bytes: usize) -> Self {
	Self {
		batch: WriteBatch::with_capacity_bytes(capacity_bytes),
		engine: engine.clone(),
	}
}

/// Queues raw key and value pairs for one map from a single pass.
///
/// The database codec is not applied, and the write batch copies each supplied
/// byte sequence. Empty input produces an empty transaction whose execution is
/// a no-op.
#[implement(Txn)]
pub fn insert<I, K, V>(map: &Map, items: I) -> Self
where
	I: IntoIterator<Item = (K, V)>,
	K: AsRef<[u8]>,
	V: AsRef<[u8]>,
{
	items
		.into_iter()
		.fold(Self::new(map.engine()), |mut txn, (key, val)| {
			txn.insert_raw(map, key, val);
			txn
		})
}

/// Queues a raw slice for one map with a precomputed capacity estimate.
///
/// The estimate includes payload lengths and worst-case record overhead before
/// the items are copied into the write batch. Empty input produces an empty
/// transaction.
#[implement(Txn)]
pub fn insert_slice<K, V>(map: &Map, items: &[(K, V)]) -> Self
where
	K: AsRef<[u8]>,
	V: AsRef<[u8]>,
{
	let capacity_bytes = size_hint(items.iter().map(|(key, val)| (key, val)));

	items.iter().fold(
		Self::with_capacity_bytes(map.engine(), capacity_bytes),
		|mut txn, (key, val)| {
			txn.insert_raw(map, key, val);
			txn
		},
	)
}

/// Queues raw entries across maps from a nonempty single pass.
///
/// The first item selects the database engine, and every subsequent map must
/// belong to that same engine. The database codec is not applied to keys or
/// values.
///
/// # Panics
///
/// Panics when `items` is empty or when any map belongs to a different database
/// engine.
#[implement(Txn)]
pub fn insert_each<'a, I, K, V>(items: I) -> Self
where
	I: IntoIterator<Item = (&'a Map, K, V)>,
	K: AsRef<[u8]>,
	V: AsRef<[u8]>,
{
	let mut items = items.into_iter();
	let (map, key, val) = items
		.next()
		.expect("insert_each: at least one item");

	let mut txn = Self::new(map.engine());

	txn.insert_raw(map, key, val);
	txn.extend(items);

	txn
}

/// Queues a nonempty raw slice across maps with a capacity estimate.
///
/// The first item selects the database engine, and every map must belong to
/// that same engine. The database codec is not applied to keys or values.
///
/// # Panics
///
/// Panics when `items` is empty or when any map belongs to a different database
/// engine.
#[implement(Txn)]
pub fn insert_each_slice<K, V>(items: &[(&Map, K, V)]) -> Self
where
	K: AsRef<[u8]>,
	V: AsRef<[u8]>,
{
	let map = items
		.first()
		.expect("insert_each_slice: at least one item")
		.0;

	let capacity_bytes = size_hint(items.iter().map(|(_, key, val)| (key, val)));

	let mut txn = Self::with_capacity_bytes(map.engine(), capacity_bytes);

	txn.extend(
		items
			.iter()
			.map(|(map, key, val)| (*map, key, val)),
	);

	txn
}

/// Serializes and queues entries across maps from a nonempty pass.
///
/// The first item selects the database engine, and every map must belong to
/// that same engine. All keys and values are encoded with the database record
/// codec before being copied into the batch.
///
/// # Panics
///
/// Panics when `items` is empty, a map belongs to another database engine, or
/// serialization of a key or value fails.
#[implement(Txn)]
pub fn put_each<'a, I, K, V>(items: I) -> Self
where
	I: IntoIterator<Item = (&'a Map, K, V)>,
	K: Serialize + Debug,
	V: Serialize,
{
	let mut items = items.into_iter();
	let (map, key, val) = items.next().expect("put_each: at least one item");
	let txn = Self::new(map.engine());

	once((map, key, val))
		.chain(items)
		.fold(txn, |mut txn, (map, key, val)| {
			txn.put(map, key, val);
			txn
		})
}

/// Serializes and queues one insertion.
///
/// The key and value use the database record codec, and the operation remains
/// pending until [`Txn::execute`]. The map must belong to the transaction's
/// database engine.
///
/// # Panics
///
/// Panics when the map belongs to another database engine or serialization of
/// the key or value fails.
#[implement(Txn)]
pub fn put<K, V>(&mut self, map: &Map, key: K, val: V)
where
	K: Serialize + Debug,
	V: Serialize,
{
	self.assert_map(map);

	let key = serialize_key(key).expect("failed to serialize batch key");
	let val = serialize_val(val).expect("failed to serialize batch val");

	self.batch.put_cf(&map.cf(), key, val);
}

/// Serializes the key and queues one raw-value insertion.
///
/// The key uses the database record codec, while the value bytes are copied
/// unchanged into the batch. The operation remains pending until
/// [`Txn::execute`], and the map must belong to the transaction's database
/// engine.
///
/// # Panics
///
/// Panics when the map belongs to another database engine or serialization of
/// the key fails.
#[implement(Txn)]
pub fn put_raw<K, V>(&mut self, map: &Map, key: K, val: V)
where
	K: Serialize + Debug,
	V: AsRef<[u8]>,
{
	self.assert_map(map);

	let key = serialize_key(key).expect("failed to serialize batch key");

	self.batch.put_cf(&map.cf(), key, val);
}

/// Queues one raw-key insertion after serializing the value.
///
/// The key bytes are copied unchanged into the batch, while the value uses the
/// database record codec. The operation remains pending until [`Txn::execute`],
/// and the map must belong to the transaction's database engine.
///
/// # Panics
///
/// Panics when the map belongs to another database engine or serialization of
/// the value fails.
#[implement(Txn)]
pub fn raw_put<K, V>(&mut self, map: &Map, key: K, val: V)
where
	K: AsRef<[u8]>,
	V: Serialize,
{
	self.assert_map(map);

	let val = serialize_val(val).expect("failed to serialize batch val");

	self.batch.put_cf(&map.cf(), key, val);
}

/// Serializes and queues one deletion.
///
/// The key uses the database record codec, and the operation remains pending
/// until [`Txn::execute`]. The map must belong to the transaction's database
/// engine.
///
/// # Panics
///
/// Panics when the map belongs to another database engine or serialization of
/// the key fails.
#[implement(Txn)]
pub fn del<K>(&mut self, map: &Map, key: K)
where
	K: Serialize + Debug,
{
	self.assert_map(map);

	let key = serialize_key(key).expect("failed to serialize batch key");

	self.batch.delete_cf(&map.cf(), key);
}

/// Queues one deletion for an already serialized key.
///
/// The key bytes are copied into the write batch without invoking the database
/// codec. The map must belong to the transaction's database engine.
///
/// # Panics
///
/// Panics when the map belongs to another database engine.
#[implement(Txn)]
pub fn del_raw<K>(&mut self, map: &Map, key: K)
where
	K: AsRef<[u8]>,
{
	self.assert_map(map);
	self.batch.delete_cf(&map.cf(), key);
}

/// Commits the batch atomically, flushes unless corked, and notifies matching
/// watchers.
///
/// An empty transaction returns without touching the engine. For a nonempty
/// batch, notifications occur only after the write and any required flush
/// succeed.
///
/// # Panics
///
/// Panics when RocksDB rejects the batch write or when the required database
/// flush fails.
#[implement(Txn)]
#[tracing::instrument(
	level = "trace",
	skip_all,
	fields(
		ops = self.len(),
		bytes = self.size_in_bytes(),
	)
)]
pub fn execute(self) {
	if self.is_empty() {
		return;
	}

	self.engine
		.db
		.write_opt(&self.batch, &self.engine.write_options)
		.or_else(or_else)
		.expect("database transaction execute error");

	if !self.engine.corked() {
		self.engine.flush().expect("database flush error");
	}

	self.notify();
}

/// Notifies watchers after a successful commit for queued keys that resolve to
/// catalog maps.
///
/// Keys are parsed lazily from the batch representation and consumed in queue
/// order. Operations without a live map in the engine's startup catalog are
/// skipped.
#[implement(Txn)]
fn notify(&self) {
	for (map, key) in self.keys() {
		map.notify(key);
	}
}

/// Iterate queued put and delete keys in insertion order.
///
/// The iterator borrows keys directly from the serialized write batch without
/// materializing a container. Keys whose column families are outside the
/// startup map catalog are omitted.
///
/// # Panics
///
/// Iteration panics if a record has an unsupported operation tag, is truncated,
/// or contains a varint whose fifth byte retains its continuation bit.
#[implement(Txn)]
pub fn keys(&self) -> impl Iterator<Item = (Arc<Map>, &[u8])> + '_ {
	let data = self.batch.data();

	Keys {
		engine: &self.engine,
		data: data.get(HEADER..).unwrap_or_default(),
	}
}

/// Returns the number of operations queued in the batch.
///
/// Both insertions and deletions count as one operation. Inspecting the count
/// does not execute the transaction.
#[implement(Txn)]
#[inline]
#[must_use]
pub fn len(&self) -> usize { self.batch.len() }

/// Reports whether the batch contains no queued operations.
///
/// A newly created or cleared transaction is empty. Executing an empty
/// transaction performs no database work.
#[implement(Txn)]
#[inline]
#[must_use]
pub fn is_empty(&self) -> bool { self.batch.is_empty() }

/// Returns the encoded size of the RocksDB write batch in bytes.
///
/// The size includes batch metadata and queued record data. Inspecting it does
/// not execute the transaction.
#[implement(Txn)]
#[inline]
#[must_use]
pub fn size_in_bytes(&self) -> usize { self.batch.size_in_bytes() }

/// Removes every queued operation from the transaction.
///
/// The captured database engine remains attached, so the transaction can be
/// populated again. Executing it before another operation is queued is a no-op.
#[implement(Txn)]
#[inline]
pub fn clear(&mut self) { self.batch.clear(); }

/// Queue one unencoded key and value after enforcing map ownership.
///
/// Both byte sequences are copied into the write batch without invoking the
/// database codec. The operation remains pending until [`Txn::execute`].
///
/// # Panics
///
/// Panics when the map belongs to another database engine.
#[implement(Txn)]
pub fn insert_raw<K, V>(&mut self, map: &Map, key: K, val: V)
where
	K: AsRef<[u8]>,
	V: AsRef<[u8]>,
{
	self.assert_map(map);
	self.batch.put_cf(&map.cf(), key, val);
}

/// Verifies that a map belongs to the transaction's database engine.
///
/// RocksDB identifies column families numerically within one database, so
/// accepting a foreign map could target a same-numbered column family in the
/// captured engine.
///
/// # Panics
///
/// Panics when `map` belongs to a different database engine.
#[implement(Txn)]
#[inline]
fn assert_map(&self, map: &Map) {
	assert!(
		Arc::ptr_eq(&self.engine, map.engine()),
		"transaction map belongs to a different database"
	);
}

impl<'a> Iterator for Keys<'a> {
	type Item = (Arc<Map>, &'a [u8]);

	fn next(&mut self) -> Option<Self::Item> {
		while !self.data.is_empty() {
			let (cf_id, key) =
				next_record(&mut self.data).expect("malformed write batch representation");

			if let Some(map) = self.engine.map_by_cf_id(cf_id) {
				return Some((map, key));
			}
		}

		None
	}
}

/// Extends this transaction with raw insertions across maps.
///
/// Each tuple queues its raw key and value through [`Txn::insert_raw`]. Use
/// [`Txn::put_each`] when the keys and values need serialization. Every map
/// must belong to the transaction's database engine.
///
/// # Panics
///
/// Panics when any map belongs to another database engine.
impl<'a, K, V> Extend<(&'a Map, K, V)> for Txn
where
	K: AsRef<[u8]>,
	V: AsRef<[u8]>,
{
	fn extend<I>(&mut self, items: I)
	where
		I: IntoIterator<Item = (&'a Map, K, V)>,
	{
		for (map, key, val) in items {
			self.insert_raw(map, key, val);
		}
	}
}

/// Extends this transaction with raw-key deletions across maps.
///
/// Each tuple queues its raw key through [`Txn::del_raw`]. Every map must
/// belong to the transaction's database engine.
///
/// # Panics
///
/// Panics when any map belongs to another database engine.
impl<'a, K> Extend<(&'a Map, K)> for Txn
where
	K: AsRef<[u8]>,
{
	fn extend<I>(&mut self, items: I)
	where
		I: IntoIterator<Item = (&'a Map, K)>,
	{
		for (map, key) in items {
			self.del_raw(map, key);
		}
	}
}

/// Decodes one record into its column family identifier and borrowed key.
///
/// Value payloads are skipped after their lengths are consumed. Unsupported
/// tags, truncated fields, and varints whose fifth byte retains its
/// continuation bit return `None` and may leave the input advanced through the
/// parsed prefix.
pub(crate) fn next_record<'a>(data: &mut &'a [u8]) -> Option<(u32, &'a [u8])> {
	let (&tag, rest) = data.split_first()?;
	*data = rest;

	let tag = Tag::try_from(tag).ok()?;

	let cf_id = match tag {
		| Tag::Value | Tag::Deletion => 0,
		| Tag::CfValue | Tag::CfDeletion => take_varint32(data)?,
	};

	let key = take_varstring(data)?;

	if matches!(tag, Tag::Value | Tag::CfValue) {
		take_varstring(data)?;
	}

	Some((cf_id, key))
}

/// Takes one length-prefixed byte string from the front of a batch record.
///
/// The returned slice borrows the original batch representation, and `data`
/// advances past it. Invalid lengths or truncated input return `None`.
fn take_varstring<'a>(data: &mut &'a [u8]) -> Option<&'a [u8]> {
	let len = take_varint32(data)?.try_into().ok()?;

	let (string, rest) = data.split_at_checked(len)?;
	*data = rest;

	Some(string)
}

/// Takes one RocksDB varint32 from the front of a batch record.
///
/// The parser consumes at most five bytes and advances `data` as bytes are
/// read. A missing byte or a continuation bit on the fifth byte returns `None`.
fn take_varint32(data: &mut &[u8]) -> Option<u32> {
	let mut result = 0_u32;

	for shift in (0_u32..32).step_by(7) {
		let (&byte, rest) = data.split_first()?;
		*data = rest;
		result |= u32::from(byte & 0x7F).checked_shl(shift)?;

		if byte & 0x80 == 0 {
			return Some(result);
		}
	}

	None
}

/// Estimates write-batch capacity for a reusable sequence of raw pairs.
///
/// The estimate includes the fixed header, worst-case per-operation metadata,
/// and payload lengths. Saturating arithmetic prevents an oversized input from
/// wrapping the reservation.
fn size_hint<'a, K, V, I>(items: I) -> usize
where
	I: Iterator<Item = (&'a K, &'a V)>,
	K: AsRef<[u8]> + 'a,
	V: AsRef<[u8]> + 'a,
{
	items.fold(HEADER, |capacity_bytes, (key, val)| {
		capacity_bytes
			.saturating_add(PER_OP)
			.saturating_add(key.as_ref().len())
			.saturating_add(val.as_ref().len())
	})
}
