use std::{fmt::Debug, sync::Arc};

use futures::{Stream, StreamExt, TryStreamExt, future};
use serde::{Deserialize, Serialize};
use tuwunel_core::{Result, implement};

use crate::keyval::{Key, result_deserialize_key, serialize_key};

/// Streams deserialized keys matching a serialized prefix in ascending order.
///
/// The scan begins at the encoded prefix and stops at the first nonmatching
/// key. Any borrowed key must not be retained across another poll.
///
/// # Panics
///
/// Panics if the prefix cannot be serialized.
#[implement(super::Map)]
pub fn keys_prefix<'a, K, P>(
	self: &'a Arc<Self>,
	prefix: &P,
) -> impl Stream<Item = Result<Key<'_, K>>> + Send + use<'a, K, P>
where
	P: Serialize + ?Sized + Debug,
	K: Deserialize<'a> + Send,
{
	self.keys_prefix_raw(prefix)
		.map(result_deserialize_key::<K>)
}

/// Streams raw keys matching a serialized prefix in ascending order.
///
/// The scan begins at the encoded prefix and stops at the first nonmatching
/// key. Yielded keys borrow cursor storage and must not be retained across
/// another poll.
///
/// # Panics
///
/// Panics if the prefix cannot be serialized.
#[implement(super::Map)]
#[tracing::instrument(skip(self), level = "trace")]
pub fn keys_prefix_raw<P>(
	self: &Arc<Self>,
	prefix: &P,
) -> impl Stream<Item = Result<Key<'_>>> + Send + use<'_, P>
where
	P: Serialize + ?Sized + Debug,
{
	let key = serialize_key(prefix).expect("failed to serialize query key");
	self.raw_keys_from(&key)
		.try_take_while(move |k: &Key<'_>| future::ok(k.starts_with(&key)))
}

/// Streams deserialized keys matching a raw prefix in ascending order.
///
/// The supplied bytes are used directly for the seek and prefix test. Any
/// borrowed key must not be retained across another poll.
#[implement(super::Map)]
pub fn keys_raw_prefix<'a, K, P>(
	self: &'a Arc<Self>,
	prefix: &'a P,
) -> impl Stream<Item = Result<Key<'_, K>>> + Send + 'a
where
	P: AsRef<[u8]> + ?Sized + Debug + Sync + 'a,
	K: Deserialize<'a> + Send + 'a,
{
	self.raw_keys_prefix(prefix)
		.map(result_deserialize_key::<K>)
}

/// Streams raw keys matching a raw prefix in ascending order.
///
/// The supplied bytes are used directly for the seek and prefix test. Yielded
/// keys borrow cursor storage and must not be retained across another poll.
#[implement(super::Map)]
pub fn raw_keys_prefix<'a, P>(
	self: &'a Arc<Self>,
	prefix: &'a P,
) -> impl Stream<Item = Result<Key<'_>>> + Send + 'a
where
	P: AsRef<[u8]> + ?Sized + Debug + Sync + 'a,
{
	self.raw_keys_from(prefix)
		.try_take_while(|k: &Key<'_>| future::ok(k.starts_with(prefix.as_ref())))
}
