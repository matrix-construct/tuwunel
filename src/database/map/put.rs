//! Serialize and Insert a Key+Value into the database.
//!
//! Overloads are provided for the user to choose the most efficient
//! serialization. When no serialization is required for both key and
//! value simply use insert() (see insert.rs).

use std::{fmt::Debug, io::Write};

use serde::Serialize;
use tuwunel_core::{arrayvec::ArrayVec, implement};

use crate::{
	keyval::{KeyBuf, ValBuf},
	ser,
};

/// Stores a serialized key and serialized value using owned buffers.
///
/// Both values pass through the database serializer before raw insertion.
/// Matching watchers are notified after RocksDB accepts the write.
///
/// # Panics
///
/// Panics if either value cannot be serialized, RocksDB rejects the write, or
/// an uncorked flush fails.
#[implement(super::Map)]
#[inline]
pub fn put<K, V>(&self, key: K, val: V)
where
	K: Serialize + Debug,
	V: Serialize,
{
	let mut key_buf = KeyBuf::new();
	let mut val_buf = ValBuf::new();
	self.bput(key, val, (&mut key_buf, &mut val_buf));
}

/// Stores a serialized key and raw value using an owned key buffer.
///
/// The key passes through the database serializer before raw insertion.
/// Matching watchers are notified after RocksDB accepts the write.
///
/// # Panics
///
/// Panics if the key cannot be serialized, RocksDB rejects the write, or an
/// uncorked flush fails.
#[implement(super::Map)]
#[inline]
pub fn put_raw<K, V>(&self, key: K, val: V)
where
	K: Serialize + Debug,
	V: AsRef<[u8]>,
{
	let mut key_buf = KeyBuf::new();
	self.bput_raw(key, val, &mut key_buf);
}

/// Stores a raw key and serialized value using an owned value buffer.
///
/// The value passes through the database serializer before raw insertion.
/// Matching watchers are notified after RocksDB accepts the write.
///
/// # Panics
///
/// Panics if the value cannot be serialized, RocksDB rejects the write, or an
/// uncorked flush fails.
#[implement(super::Map)]
#[inline]
pub fn raw_put<K, V>(&self, key: K, val: V)
where
	K: AsRef<[u8]>,
	V: Serialize,
{
	let mut val_buf = ValBuf::new();
	self.raw_bput(key, val, &mut val_buf);
}

/// Stores a serialized key and value with fixed-capacity value storage.
///
/// The key uses an owned buffer, while `VMAX` bounds the complete encoded value
/// without a heap fallback. Matching watchers are notified after RocksDB
/// accepts the write.
///
/// # Panics
///
/// Panics if serialization fails, the encoded value exceeds `VMAX`, RocksDB
/// rejects the write, or an uncorked flush fails.
#[implement(super::Map)]
#[inline]
pub fn put_aput<const VMAX: usize, K, V>(&self, key: K, val: V)
where
	K: Serialize + Debug,
	V: Serialize,
{
	let mut key_buf = KeyBuf::new();
	let mut val_buf = ArrayVec::<u8, VMAX>::new();
	self.bput(key, val, (&mut key_buf, &mut val_buf));
}

/// Stores a serialized key and value with fixed-capacity key storage.
///
/// `KMAX` bounds the complete encoded key without a heap fallback, while the
/// value uses an owned buffer. Matching watchers are notified after RocksDB
/// accepts the write.
///
/// # Panics
///
/// Panics if serialization fails, the encoded key exceeds `KMAX`, RocksDB
/// rejects the write, or an uncorked flush fails.
#[implement(super::Map)]
#[inline]
pub fn aput_put<const KMAX: usize, K, V>(&self, key: K, val: V)
where
	K: Serialize + Debug,
	V: Serialize,
{
	let mut key_buf = ArrayVec::<u8, KMAX>::new();
	let mut val_buf = ValBuf::new();
	self.bput(key, val, (&mut key_buf, &mut val_buf));
}

/// Stores a serialized key and value using fixed-capacity buffers.
///
/// `KMAX` and `VMAX` bound the complete encoded key and value without heap
/// fallbacks. Matching watchers are notified after RocksDB accepts the write.
///
/// # Panics
///
/// Panics if serialization fails, either encoded value exceeds its capacity,
/// RocksDB rejects the write, or an uncorked flush fails.
#[implement(super::Map)]
#[inline]
pub fn aput<const KMAX: usize, const VMAX: usize, K, V>(&self, key: K, val: V)
where
	K: Serialize + Debug,
	V: Serialize,
{
	let mut key_buf = ArrayVec::<u8, KMAX>::new();
	let mut val_buf = ArrayVec::<u8, VMAX>::new();
	self.bput(key, val, (&mut key_buf, &mut val_buf));
}

/// Stores a serialized key and raw value with fixed-capacity key storage.
///
/// `KMAX` bounds the complete encoded key without a heap fallback. Matching
/// watchers are notified after RocksDB accepts the write.
///
/// # Panics
///
/// Panics if serialization fails, the encoded key exceeds `KMAX`, RocksDB
/// rejects the write, or an uncorked flush fails.
#[implement(super::Map)]
#[inline]
pub fn aput_raw<const KMAX: usize, K, V>(&self, key: K, val: V)
where
	K: Serialize + Debug,
	V: AsRef<[u8]>,
{
	let mut key_buf = ArrayVec::<u8, KMAX>::new();
	self.bput_raw(key, val, &mut key_buf);
}

/// Stores a raw key and serialized value with fixed-capacity value storage.
///
/// `VMAX` bounds the complete encoded value without a heap fallback. Matching
/// watchers are notified after RocksDB accepts the write.
///
/// # Panics
///
/// Panics if serialization fails, the encoded value exceeds `VMAX`, RocksDB
/// rejects the write, or an uncorked flush fails.
#[implement(super::Map)]
#[inline]
pub fn raw_aput<const VMAX: usize, K, V>(&self, key: K, val: V)
where
	K: AsRef<[u8]>,
	V: Serialize,
{
	let mut val_buf = ArrayVec::<u8, VMAX>::new();
	self.raw_bput(key, val, &mut val_buf);
}

/// Stores a serialized key and value using caller-supplied buffers.
///
/// Serialization appends the encoded key and value to the tuple's first and
/// second buffers, respectively. The write uses each buffer's full resulting
/// contents and then notifies matching watchers after RocksDB accepts it.
///
/// # Panics
///
/// Panics if either value cannot be serialized, RocksDB rejects the write, or
/// an uncorked flush fails.
#[implement(super::Map)]
pub fn bput<K, V, Bk, Bv>(&self, key: K, val: V, mut buf: (Bk, Bv))
where
	K: Serialize + Debug,
	V: Serialize,
	Bk: Write + AsRef<[u8]>,
	Bv: Write + AsRef<[u8]>,
{
	let val = ser::serialize(&mut buf.1, val).expect("failed to serialize insertion val");
	self.bput_raw(key, val, &mut buf.0);
}

/// Stores a serialized key and raw value using a caller-supplied key buffer.
///
/// Serialization appends the encoded key to the supplied buffer, and the write
/// uses its full resulting contents. Matching watchers are notified after
/// RocksDB accepts the write.
///
/// # Panics
///
/// Panics if the key cannot be serialized, RocksDB rejects the write, or an
/// uncorked flush fails.
#[implement(super::Map)]
#[tracing::instrument(skip(self, val, buf), level = "trace")]
pub fn bput_raw<K, V, Bk>(&self, key: K, val: V, mut buf: Bk)
where
	K: Serialize + Debug,
	V: AsRef<[u8]>,
	Bk: Write + AsRef<[u8]>,
{
	let key = ser::serialize(&mut buf, key).expect("failed to serialize insertion key");
	self.insert(&key, val);
}

/// Stores a raw key and serialized value using a caller-supplied value buffer.
///
/// Serialization appends the encoded value to the supplied buffer, and the
/// write uses its full resulting contents. Matching watchers are notified after
/// RocksDB accepts the write.
///
/// # Panics
///
/// Panics if the value cannot be serialized, RocksDB rejects the write, or an
/// uncorked flush fails.
#[implement(super::Map)]
pub fn raw_bput<K, V, Bv>(&self, key: K, val: V, mut buf: Bv)
where
	K: AsRef<[u8]>,
	V: Serialize,
	Bv: Write + AsRef<[u8]>,
{
	let val = ser::serialize(&mut buf, val).expect("failed to serialize insertion val");
	self.insert(&key, val);
}
