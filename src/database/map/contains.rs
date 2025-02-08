use std::{convert::AsRef, fmt::Debug, future::Future, io::Write, sync::Arc};

use conduwuit::{
	arrayvec::ArrayVec,
	err, implement,
	utils::{future::TryExtExt, result::FlatOk},
	Result,
};
use futures::FutureExt;
use serde::Serialize;

use crate::{keyval::KeyBuf, ser};

/// Returns true if the map contains the key.
/// - key is serialized into allocated buffer
/// - harder errors may not be reported
#[inline]
#[implement(super::Map)]
pub fn contains<K>(self: &Arc<Self>, key: &K) -> impl Future<Output = bool> + Send + '_
where
	K: Serialize + ?Sized + Debug,
{
	let mut buf = KeyBuf::new();
	self.bcontains(key, &mut buf)
}

/// Returns true if the map contains the key.
/// - key is serialized into stack-buffer
/// - harder errors will panic
#[inline]
#[implement(super::Map)]
pub fn acontains<const MAX: usize, K>(
	self: &Arc<Self>,
	key: &K,
) -> impl Future<Output = bool> + Send + '_
where
	K: Serialize + ?Sized + Debug,
{
	let mut buf = ArrayVec::<u8, MAX>::new();
	self.bcontains(key, &mut buf)
}

/// Returns true if the map contains the key.
/// - key is serialized into provided buffer
/// - harder errors will panic
#[implement(super::Map)]
#[tracing::instrument(skip(self, buf), fields(%self), level = "trace")]
pub fn bcontains<K, B>(
	self: &Arc<Self>,
	key: &K,
	buf: &mut B,
) -> impl Future<Output = bool> + Send + '_
where
	K: Serialize + ?Sized + Debug,
	B: Write + AsRef<[u8]>,
{
	let key = ser::serialize(buf, key).expect("failed to serialize query key");
	self.exists(key).is_ok()
}

/// Returns Ok if the map contains the key.
/// - key is raw
#[inline]
#[implement(super::Map)]
pub fn exists<'a, K>(self: &'a Arc<Self>, key: &K) -> impl Future<Output = Result> + Send + 'a
where
	K: AsRef<[u8]> + ?Sized + Debug + 'a,
{
	self.get(key).map(|res| res.map(|_| ()))
}

/// Returns Ok if the map contains the key; NotFound otherwise. Harder errors
/// may not always be reported properly.
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

/// Rocksdb limits this to kBlockCacheTier internally so this is not actually a
/// blocking call; in case that changes we set this as well in our read_options.
#[implement(super::Map)]
pub(crate) fn maybe_exists<K>(&self, key: &K) -> bool
where
	K: AsRef<[u8]> + ?Sized,
{
	self.db
		.db
		.key_may_exist_cf_opt(&self.cf(), key, &self.cache_read_options)
}
