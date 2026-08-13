use std::{fmt::Debug, sync::Arc};

use futures::{Stream, StreamExt};
use rocksdb::Direction;
use serde::{Deserialize, Serialize};
use tuwunel_core::{Result, implement};

use super::seek::seek_stream;
use crate::{
	keyval::{Key, result_deserialize_key, serialize_key},
	stream,
};

/// Streams deserialized keys forward from a serialized lower bound.
///
/// The scan begins at the first key not less than the encoded bound. Any
/// borrowed key must not be retained across another poll.
///
/// # Panics
///
/// Panics if the lower bound cannot be serialized.
#[implement(super::Map)]
pub fn keys_from<'a, K, P>(
	self: &'a Arc<Self>,
	from: &P,
) -> impl Stream<Item = Result<Key<'_, K>>> + Send + use<'a, K, P>
where
	P: Serialize + ?Sized + Debug,
	K: Deserialize<'a> + Send,
{
	self.keys_from_raw(from)
		.map(result_deserialize_key::<K>)
}

/// Streams raw keys forward from a serialized lower bound.
///
/// The scan begins at the first key not less than the encoded bound. Yielded
/// keys borrow cursor storage and must not be retained across another poll.
///
/// # Panics
///
/// Panics if the lower bound cannot be serialized.
#[implement(super::Map)]
#[tracing::instrument(skip(self), level = "trace")]
pub fn keys_from_raw<P>(
	self: &Arc<Self>,
	from: &P,
) -> impl Stream<Item = Result<Key<'_>>> + Send + use<'_, P>
where
	P: Serialize + ?Sized + Debug,
{
	let key = serialize_key(from).expect("failed to serialize query key");
	self.raw_keys_from(&key)
}

/// Streams deserialized keys forward from a raw lower bound.
///
/// The supplied bytes are used directly as the seek position. Any borrowed key
/// must not be retained across another poll.
#[implement(super::Map)]
pub fn keys_raw_from<'a, K, P>(
	self: &'a Arc<Self>,
	from: &P,
) -> impl Stream<Item = Result<Key<'_, K>>> + Send + use<'a, K, P>
where
	P: AsRef<[u8]> + ?Sized + Debug + Sync,
	K: Deserialize<'a> + Send,
{
	self.raw_keys_from(from)
		.map(result_deserialize_key::<K>)
}

/// Streams raw keys forward from a raw lower bound.
///
/// The supplied bytes are used directly as the seek position. Yielded keys
/// borrow cursor storage and must not be retained across another poll.
#[implement(super::Map)]
#[tracing::instrument(skip(self, from), fields(%self), level = "trace")]
pub fn raw_keys_from<P>(
	self: &Arc<Self>,
	from: &P,
) -> impl Stream<Item = Result<Key<'_>>> + Send + use<'_, P>
where
	P: AsRef<[u8]> + ?Sized + Debug,
{
	seek_stream::<stream::Keys<'_>, _>(self, Direction::Forward, Some(from.as_ref()))
}
