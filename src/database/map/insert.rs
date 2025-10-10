//! Insert a Key+Value into the database.
//!
//! Overloads are provided for the user to choose the most efficient
//! serialization or bypass for pre=serialized (raw) inputs.

use std::{convert::AsRef, fmt::Debug};

use rocksdb::WriteBatchWithTransaction;
use tuwunel_core::implement;

use crate::util::or_else;

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
	self.engine
		.db
		.put_cf_opt(&self.cf(), key, val, write_options)
		.or_else(or_else)
		.expect("database insert error");

	if !self.engine.corked() {
		self.engine.flush().expect("database flush error");
	}

	self.notify(key.as_ref());
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
	self.engine
		.db
		.write_opt(batch, write_options)
		.or_else(or_else)
		.expect("database insert batch error");

	if !self.engine.corked() {
		self.engine.flush().expect("database flush error");
	}
}

/// Atomically write a batch of raw put and delete operations.
#[implement(super::Map)]
#[tracing::instrument(skip(self, puts, dels), fields(%self), level = "trace")]
pub fn write_batch_raw<Ip, Ik>(&self, puts: Ip, dels: Ik)
where
	Ip: IntoIterator<Item = (Vec<u8>, Vec<u8>)>,
	Ik: IntoIterator<Item = Vec<u8>>,
{
	let mut batch = WriteBatchWithTransaction::<false>::default();
	let cf = self.cf();
	for (k, v) in puts {
		batch.put_cf(&cf, &k, &v);
	}
	for k in dels {
		batch.delete_cf(&cf, &k);
	}

	let write_options = &self.write_options;
	use crate::util::or_else as db_or_else;
	self.engine
		.db
		.write_opt(batch, write_options)
		.or_else(db_or_else)
		.expect("database write batch error");

	if !self.engine.corked() {
		self.engine.flush().expect("database flush error");
	}
}
