use std::sync::Arc;

use futures::{Stream, StreamExt};
use rocksdb::Direction;
use serde::Deserialize;
use tuwunel_core::{Result, implement};

use super::seek::seek_stream;
use crate::{keyval, keyval::KeyVal, stream};

/// Streams deserialized key-value entries in descending database order.
///
/// Each raw pair is decoded with the database deserializer. Any borrowed key or
/// value must not be retained across another poll of the stream.
#[implement(super::Map)]
pub fn rev_stream<'a, K, V>(
	self: &'a Arc<Self>,
) -> impl Stream<Item = Result<KeyVal<'_, K, V>>> + Send
where
	K: Deserialize<'a> + Send,
	V: Deserialize<'a> + Send,
{
	self.rev_raw_stream()
		.map(keyval::result_deserialize::<K, V>)
}

/// Streams raw key-value entries in descending database order.
///
/// The scan begins at the last key in the column family. Yielded keys and
/// values borrow cursor storage and must not be retained across another poll.
#[implement(super::Map)]
#[tracing::instrument(skip(self), fields(%self), level = "trace")]
pub fn rev_raw_stream(self: &Arc<Self>) -> impl Stream<Item = Result<KeyVal<'_>>> + Send {
	seek_stream::<stream::ItemsRev<'_>, _>(self, Direction::Reverse, None)
}
