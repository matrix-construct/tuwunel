//! Insert a Key+Value into the database.
//!
//! Overloads are provided for the user to choose the most efficient
//! serialization or bypass for pre=serialized (raw) inputs.

use std::{convert::AsRef, fmt::Debug, io::Write};

use conduwuit::{arrayvec::ArrayVec, implement};
use rocksdb::WriteBatchWithTransaction;
use serde::Serialize;

use crate::{
	keyval::{KeyBuf, ValBuf},
	ser,
	util::or_else,
};

/// Insert Key/Value
///
/// - Key is serialized
/// - Val is serialized
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

/// Insert Key/Value
///
/// - Key is serialized
/// - Val is raw
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

/// Insert Key/Value
///
/// - Key is raw
/// - Val is serialized
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

/// Insert Key/Value
///
/// - Key is serialized
/// - Val is serialized to stack-buffer
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

/// Insert Key/Value
///
/// - Key is serialized to stack-buffer
/// - Val is serialized
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

/// Insert Key/Value
///
/// - Key is serialized to stack-buffer
/// - Val is serialized to stack-buffer
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

/// Insert Key/Value
///
/// - Key is serialized to stack-buffer
/// - Val is raw
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

/// Insert Key/Value
///
/// - Key is raw
/// - Val is serialized to stack-buffer
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

/// Insert Key/Value
///
/// - Key is serialized to supplied buffer
/// - Val is serialized to supplied buffer
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

/// Insert Key/Value
///
/// - Key is serialized to supplied buffer
/// - Val is raw
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

/// Insert Key/Value
///
/// - Key is raw
/// - Val is serialized to supplied buffer
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

/// Insert Key/Value
///
/// - Key is raw
/// - Val is raw
#[implement(super::Map)]
#[tracing::instrument(skip_all, fields(%self), level = "trace")]
pub fn insert<K, V>(&self, key: &K, val: V)
where
	K: AsRef<[u8]> + ?Sized,
	V: AsRef<[u8]>,
{
	let write_options = &self.write_options;
	self.db
		.db
		.put_cf_opt(&self.cf(), key, val, write_options)
		.or_else(or_else)
		.expect("database insert error");

	if !self.db.corked() {
		self.db.flush().expect("database flush error");
	}

	self.watchers.wake(key.as_ref());
}

#[implement(super::Map)]
#[tracing::instrument(skip(self, iter), fields(%self), level = "trace")]
pub fn insert_batch<'a, I, K, V>(&'a self, iter: I)
where
	I: Iterator<Item = (K, V)> + Send + Debug,
	K: AsRef<[u8]> + Sized + Debug + 'a,
	V: AsRef<[u8]> + Sized + 'a,
{
	let mut batch = WriteBatchWithTransaction::<false>::default();
	for (key, val) in iter {
		batch.put_cf(&self.cf(), key.as_ref(), val.as_ref());
	}

	let write_options = &self.write_options;
	self.db
		.db
		.write_opt(batch, write_options)
		.or_else(or_else)
		.expect("database insert batch error");

	if !self.db.corked() {
		self.db.flush().expect("database flush error");
	}
}
