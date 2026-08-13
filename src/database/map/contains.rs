use std::{fmt::Debug, io::Write, sync::Arc};

use futures::FutureExt;
use serde::Serialize;
use tuwunel_core::{
	Result,
	arrayvec::ArrayVec,
	err, implement,
	utils::{future::TryExtExt, result::FlatOk},
};

use crate::{keyval::KeyBuf, ser};

/// Checks whether a serialized key exists.
///
/// The key is encoded into an owned buffer before asynchronous raw-key lookup.
/// Missing keys and database errors are folded into `false`.
///
/// # Panics
///
/// Panics if the key cannot be serialized.
#[inline]
#[implement(super::Map)]
pub fn contains<K>(
	self: &Arc<Self>,
	key: &K,
) -> impl Future<Output = bool> + Send + '_ + use<'_, K>
where
	K: Serialize + ?Sized + Debug,
{
	let mut buf = KeyBuf::new();
	self.bcontains(key, &mut buf)
}

/// Checks whether a serialized key exists using fixed-capacity storage.
///
/// `MAX` bounds the complete encoded key without a heap fallback. Missing keys
/// and database errors are folded into `false`.
///
/// # Panics
///
/// Panics if the encoded key exceeds `MAX` or serialization otherwise fails.
#[inline]
#[implement(super::Map)]
pub fn acontains<const MAX: usize, K>(
	self: &Arc<Self>,
	key: &K,
) -> impl Future<Output = bool> + Send + '_ + use<'_, MAX, K>
where
	K: Serialize + ?Sized + Debug,
{
	let mut buf = ArrayVec::<u8, MAX>::new();
	self.bcontains(key, &mut buf)
}

/// Checks whether a serialized key exists using a caller-supplied buffer.
///
/// Serialization appends to the supplied buffer, and lookup uses its full
/// resulting contents. Missing keys and database errors are folded into
/// `false`.
///
/// # Panics
///
/// Panics if the key cannot be serialized.
#[implement(super::Map)]
#[tracing::instrument(skip(self, buf), fields(%self), level = "trace")]
pub fn bcontains<K, B>(
	self: &Arc<Self>,
	key: &K,
	buf: &mut B,
) -> impl Future<Output = bool> + Send + '_ + use<'_, K, B>
where
	K: Serialize + ?Sized + Debug,
	B: Write + AsRef<[u8]>,
{
	let key = ser::serialize(buf, key).expect("failed to serialize query key");
	self.exists(key).is_ok()
}

/// Checks whether a raw key exists asynchronously.
///
/// Success returns unit, while missing keys and database failures remain
/// errors. The lookup uses the same cache-first path as `get`.
#[inline]
#[implement(super::Map)]
pub fn exists<'a, K>(
	self: &'a Arc<Self>,
	key: &K,
) -> impl Future<Output = Result> + Send + 'a + use<'a, K>
where
	K: AsRef<[u8]> + ?Sized + Debug + 'a,
{
	self.get(key).map(|res| res.map(|_| ()))
}

/// Checks synchronously whether a raw key exists.
///
/// A cache-tier existence hint can avoid a point read when absence is certain.
/// Missing keys return the map's not-found error, while database failures
/// remain errors.
#[implement(super::Map)]
#[tracing::instrument(skip(self, key), fields(%self), level = "trace")]
pub fn exists_blocking<K>(&self, key: &K) -> Result
where
	K: AsRef<[u8]> + ?Sized + Debug,
{
	self.maybe_exists(key)
		.then(|| self.get_blocking(key))
		.flat_ok()
		.map(|_| ())
		.ok_or_else(|| err!(Request(NotFound("Not found in database"))))
}

/// Tests whether RocksDB can rule out a raw key without storage I/O.
///
/// A `false` result proves absence, while `true` still requires a point read.
/// The configured read options restrict the probe to block cache.
#[implement(super::Map)]
pub(crate) fn maybe_exists<K>(&self, key: &K) -> bool
where
	K: AsRef<[u8]> + ?Sized,
{
	self.engine
		.db
		.key_may_exist_cf_opt(&self.cf(), key, &self.cache_read_options)
}
