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

#[implement(Txn)]
pub fn new(engine: &Arc<Engine>) -> Self {
	Self {
		batch: WriteBatch::default(),
		engine: engine.clone(),
	}
}

#[implement(Txn)]
pub fn with_capacity_bytes(engine: &Arc<Engine>, capacity_bytes: usize) -> Self {
	Self {
		batch: WriteBatch::with_capacity_bytes(capacity_bytes),
		engine: engine.clone(),
	}
}

/// Queue raw key and value pairs for one column family from a single pass.
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

/// Queue a raw slice for one column family with an exact payload estimate.
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

/// Queue raw entries across column families from a nonempty single pass.
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

	let txn = Self::new(map.engine());

	once((map, key, val))
		.chain(items)
		.fold(txn, |mut txn, (map, key, val)| {
			txn.insert_raw(map, key, val);
			txn
		})
}

/// Queue a nonempty raw slice across column families with a payload estimate.
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

	items.iter().fold(
		Self::with_capacity_bytes(map.engine(), capacity_bytes),
		|mut txn, (map, key, val)| {
			txn.insert_raw(map, key, val);
			txn
		},
	)
}

/// Serialize and queue entries across column families from a nonempty pass.
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

/// Serialize and queue one insertion.
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

/// Serialize and queue one deletion.
#[implement(Txn)]
pub fn del<K>(&mut self, map: &Map, key: K)
where
	K: Serialize + Debug,
{
	self.assert_map(map);

	let key = serialize_key(key).expect("failed to serialize batch key");

	self.batch.delete_cf(&map.cf(), key);
}

/// Queue one deletion for an already serialized key.
#[implement(Txn)]
pub fn del_raw<K>(&mut self, map: &Map, key: K)
where
	K: AsRef<[u8]>,
{
	self.assert_map(map);
	self.batch.delete_cf(&map.cf(), key);
}

/// Commit atomically, flush unless corked, and notify matching watchers.
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

#[implement(Txn)]
fn notify(&self) {
	for (map, key) in self.keys() {
		map.notify(key);
	}
}

/// Iterate queued put and delete keys in insertion order.
///
/// Keys whose column families are outside the startup map catalog are omitted.
#[implement(Txn)]
pub fn keys(&self) -> impl Iterator<Item = (Arc<Map>, &[u8])> + '_ {
	let data = self.batch.data();

	Keys {
		engine: &self.engine,
		data: data.get(HEADER..).unwrap_or_default(),
	}
}

#[implement(Txn)]
#[inline]
#[must_use]
pub fn len(&self) -> usize { self.batch.len() }

#[implement(Txn)]
#[inline]
#[must_use]
pub fn is_empty(&self) -> bool { self.batch.is_empty() }

#[implement(Txn)]
#[inline]
#[must_use]
pub fn size_in_bytes(&self) -> usize { self.batch.size_in_bytes() }

#[implement(Txn)]
#[inline]
pub fn clear(&mut self) { self.batch.clear(); }

/// Queue one unencoded key and value after enforcing map ownership.
#[implement(Txn)]
pub fn insert_raw<K, V>(&mut self, map: &Map, key: K, val: V)
where
	K: AsRef<[u8]>,
	V: AsRef<[u8]>,
{
	self.assert_map(map);
	self.batch.put_cf(&map.cf(), key, val);
}

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

/// Decode one record as its column family id and key; values are skipped.
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

fn take_varstring<'a>(data: &mut &'a [u8]) -> Option<&'a [u8]> {
	let len = take_varint32(data)?.try_into().ok()?;

	let (string, rest) = data.split_at_checked(len)?;
	*data = rest;

	Some(string)
}

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
