use std::{fmt::Debug, sync::Arc};

use futures::{Stream, StreamExt};
use rocksdb::Direction;
use serde::{Deserialize, Serialize};
use tuwunel_core::{Result, implement};

use super::seek::seek_stream;
use crate::{
	keyval::{KeyVal, result_deserialize, serialize_key},
	stream,
};

/// Iterate key-value entries in the map starting from lower-bound.
///
/// - Query is serialized
/// - Result is deserialized
#[implement(super::Map)]
pub fn stream_from<'a, K, V, P>(
	self: &'a Arc<Self>,
	from: &P,
) -> impl Stream<Item = Result<KeyVal<'_, K, V>>> + Send + use<'a, K, V, P>
where
	P: Serialize + ?Sized + Debug,
	K: Deserialize<'a> + Send,
	V: Deserialize<'a> + Send,
{
	self.stream_from_raw(from)
		.map(result_deserialize::<K, V>)
}

/// Iterate key-value entries in the map starting from lower-bound.
///
/// - Query is serialized
/// - Result is raw
#[implement(super::Map)]
#[tracing::instrument(skip(self), level = "trace")]
pub fn stream_from_raw<P>(
	self: &Arc<Self>,
	from: &P,
) -> impl Stream<Item = Result<KeyVal<'_>>> + Send + use<'_, P>
where
	P: Serialize + ?Sized + Debug,
{
	let key = serialize_key(from).expect("failed to serialize query key");
	self.raw_stream_from(&key)
}

/// Iterate key-value entries in the map starting from lower-bound.
///
/// - Query is raw
/// - Result is deserialized
#[implement(super::Map)]
pub fn stream_raw_from<'a, K, V, P>(
	self: &'a Arc<Self>,
	from: &P,
) -> impl Stream<Item = Result<KeyVal<'_, K, V>>> + Send + use<'a, K, V, P>
where
	P: AsRef<[u8]> + ?Sized + Debug + Sync,
	K: Deserialize<'a> + Send,
	V: Deserialize<'a> + Send,
{
	self.raw_stream_from(from)
		.map(result_deserialize::<K, V>)
}

/// Iterate key-value entries in the map starting from lower-bound.
///
/// - Query is raw
/// - Result is raw
#[implement(super::Map)]
#[tracing::instrument(skip(self, from), fields(%self), level = "trace")]
pub fn raw_stream_from<P>(
	self: &Arc<Self>,
	from: &P,
) -> impl Stream<Item = Result<KeyVal<'_>>> + Send + use<'_, P>
where
	P: AsRef<[u8]> + ?Sized + Debug,
{
	seek_stream::<stream::Items<'_>, _>(self, Direction::Forward, Some(from.as_ref()))
}
