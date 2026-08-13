use std::{fmt::Debug, io::Write, sync::Arc};

use serde::Serialize;
use tuwunel_core::{Result, arrayvec::ArrayVec, implement};

use crate::{Handle, keyval::KeyBuf, ser};

/// Fetches a serialized key asynchronously using an owned buffer.
///
/// The key is encoded with the database serializer before raw lookup. The
/// returned handle keeps its RocksDB value storage pinned for the handle's
/// lifetime.
///
/// # Panics
///
/// Panics if the key cannot be serialized.
#[implement(super::Map)]
#[inline]
pub fn qry<K>(
	self: &Arc<Self>,
	key: &K,
) -> impl Future<Output = Result<Handle<'_>>> + Send + use<'_, K>
where
	K: Serialize + ?Sized + Debug,
{
	let mut buf = KeyBuf::new();
	self.bqry(key, &mut buf)
}

/// Fetches a serialized key asynchronously using fixed-capacity storage.
///
/// `MAX` bounds the complete encoded key without a heap fallback. The returned
/// handle keeps its RocksDB value storage pinned for the handle's lifetime.
///
/// # Panics
///
/// Panics if the encoded key exceeds `MAX` or serialization otherwise fails.
#[implement(super::Map)]
#[inline]
pub fn aqry<const MAX: usize, K>(
	self: &Arc<Self>,
	key: &K,
) -> impl Future<Output = Result<Handle<'_>>> + Send + use<'_, MAX, K>
where
	K: Serialize + ?Sized + Debug,
{
	let mut buf = ArrayVec::<u8, MAX>::new();
	self.bqry(key, &mut buf)
}

/// Fetches a serialized key asynchronously using a caller-supplied buffer.
///
/// Serialization appends to the supplied buffer, and lookup uses its full
/// resulting contents. The returned handle keeps its RocksDB value storage
/// pinned for the handle's lifetime.
///
/// # Panics
///
/// Panics if the key cannot be serialized.
#[implement(super::Map)]
#[tracing::instrument(skip(self, buf), level = "trace")]
pub fn bqry<K, B>(
	self: &Arc<Self>,
	key: &K,
	buf: &mut B,
) -> impl Future<Output = Result<Handle<'_>>> + Send + use<'_, K, B>
where
	K: Serialize + ?Sized + Debug,
	B: Write + AsRef<[u8]>,
{
	let key = ser::serialize(buf, key).expect("failed to serialize query key");
	self.get(key)
}
