use std::{fmt::Debug, sync::Arc};

use futures::{Stream, StreamExt, TryStreamExt, future};
use serde::{Deserialize, Serialize};
use tuwunel_core::{Result, implement};

use crate::keyval::{KeyVal, result_deserialize, serialize_key};

/// Streams deserialized entries from a reverse seek at a serialized prefix.
///
/// The encoded prefix is both the seek position and the predicate. Under
/// bytewise ordering, longer keys with the prefix sort above this starting
/// point, so the scan normally reaches only an exact-key match. Any borrowed
/// key or value must not be retained across another poll.
///
/// # Panics
///
/// Panics if the prefix cannot be serialized.
#[implement(super::Map)]
pub fn rev_stream_prefix<'a, K, V, P>(
	self: &'a Arc<Self>,
	prefix: &P,
) -> impl Stream<Item = Result<KeyVal<'_, K, V>>> + Send + use<'a, K, V, P>
where
	P: Serialize + ?Sized + Debug,
	K: Deserialize<'a> + Send,
	V: Deserialize<'a> + Send,
{
	self.rev_stream_prefix_raw(prefix)
		.map(result_deserialize::<K, V>)
}

/// Streams raw entries from a reverse seek at a serialized prefix.
///
/// The encoded prefix is both the seek position and the predicate. Under
/// bytewise ordering, longer keys with the prefix sort above this starting
/// point, so the scan normally reaches only an exact-key match. Yielded keys
/// and values borrow cursor storage and must not be retained across another
/// poll.
///
/// # Panics
///
/// Panics if the prefix cannot be serialized.
#[implement(super::Map)]
#[tracing::instrument(skip(self), level = "trace")]
pub fn rev_stream_prefix_raw<P>(
	self: &Arc<Self>,
	prefix: &P,
) -> impl Stream<Item = Result<KeyVal<'_>>> + Send + use<'_, P>
where
	P: Serialize + ?Sized + Debug,
{
	let key = serialize_key(prefix).expect("failed to serialize query key");
	self.rev_raw_stream_from(&key)
		.try_take_while(move |(k, _): &KeyVal<'_>| future::ok(k.starts_with(&key)))
}

/// Streams deserialized entries from a reverse seek at a raw prefix.
///
/// The supplied bytes are both the seek position and the predicate. Under
/// bytewise ordering, longer keys with the prefix sort above this starting
/// point, so the scan normally reaches only an exact-key match. Any borrowed
/// key or value must not be retained across another poll.
#[implement(super::Map)]
pub fn rev_stream_raw_prefix<'a, K, V, P>(
	self: &'a Arc<Self>,
	prefix: &'a P,
) -> impl Stream<Item = Result<KeyVal<'_, K, V>>> + Send + 'a
where
	P: AsRef<[u8]> + ?Sized + Debug + Sync + 'a,
	K: Deserialize<'a> + Send + 'a,
	V: Deserialize<'a> + Send + 'a,
{
	self.rev_raw_stream_prefix(prefix)
		.map(result_deserialize::<K, V>)
}

/// Streams raw entries from a reverse seek at a raw prefix.
///
/// The supplied bytes are both the seek position and the predicate. Under
/// bytewise ordering, longer keys with the prefix sort above this starting
/// point, so the scan normally reaches only an exact-key match. Yielded keys
/// and values borrow cursor storage and must not be retained across another
/// poll.
#[implement(super::Map)]
pub fn rev_raw_stream_prefix<'a, P>(
	self: &'a Arc<Self>,
	prefix: &'a P,
) -> impl Stream<Item = Result<KeyVal<'_>>> + Send + 'a
where
	P: AsRef<[u8]> + ?Sized + Debug + Sync + 'a,
{
	self.rev_raw_stream_from(prefix)
		.try_take_while(|(k, _): &KeyVal<'_>| future::ok(k.starts_with(prefix.as_ref())))
}
