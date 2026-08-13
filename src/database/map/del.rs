use std::{fmt::Debug, io::Write};

use serde::Serialize;
use tuwunel_core::{arrayvec::ArrayVec, implement};

use crate::{keyval::KeyBuf, ser};

/// Deletes a serialized key using an owned buffer.
///
/// The database serializer encodes the key before raw deletion. Matching
/// watchers are notified after RocksDB accepts the removal.
///
/// # Panics
///
/// Panics if serialization fails, RocksDB rejects the deletion, or an uncorked
/// flush fails.
#[implement(super::Map)]
#[inline]
pub fn del<K>(&self, key: K)
where
	K: Serialize + Debug,
{
	let mut buf = KeyBuf::new();
	self.bdel(key, &mut buf);
}

/// Deletes a serialized key using a fixed-capacity buffer.
///
/// `MAX` bounds the complete encoded key without a heap fallback. Matching
/// watchers are notified after RocksDB accepts the removal.
///
/// # Panics
///
/// Panics if the encoded key exceeds `MAX`, serialization otherwise fails,
/// RocksDB rejects the deletion, or an uncorked flush fails.
#[implement(super::Map)]
#[inline]
pub fn adel<const MAX: usize, K>(&self, key: K)
where
	K: Serialize + Debug,
{
	let mut buf = ArrayVec::<u8, MAX>::new();
	self.bdel(key, &mut buf);
}

/// Deletes a serialized key using a caller-supplied buffer.
///
/// Serialization appends the encoded key to the supplied buffer, and deletion
/// uses its full resulting contents. Matching watchers are notified after
/// RocksDB accepts the removal.
///
/// # Panics
///
/// Panics if serialization fails, RocksDB rejects the deletion, or an uncorked
/// flush fails.
#[implement(super::Map)]
#[tracing::instrument(skip(self, buf), level = "trace")]
pub fn bdel<K, B>(&self, key: K, buf: &mut B)
where
	K: Serialize + Debug,
	B: Write + AsRef<[u8]>,
{
	let key = ser::serialize(buf, key).expect("failed to serialize deletion key");
	self.remove(key);
}
