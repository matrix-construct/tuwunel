use std::sync::Arc;

use futures::{Stream, StreamExt};
use rocksdb::Direction;
use serde::Deserialize;
use tuwunel_core::{Result, implement};

use super::seek::seek_stream;
use crate::{keyval, keyval::Key, stream};

/// Streams deserialized keys in descending database order.
///
/// Each raw key is decoded with the database deserializer. Any borrowed key
/// must not be retained across another poll.
#[implement(super::Map)]
pub fn rev_keys<'a, K>(self: &'a Arc<Self>) -> impl Stream<Item = Result<Key<'_, K>>> + Send
where
	K: Deserialize<'a> + Send,
{
	self.rev_raw_keys()
		.map(keyval::result_deserialize_key::<K>)
}

/// Streams raw keys in descending database order.
///
/// The scan begins at the last key in the column family. Yielded keys borrow
/// cursor storage and must not be retained across another poll.
#[implement(super::Map)]
#[tracing::instrument(skip(self), fields(%self), level = "trace")]
pub fn rev_raw_keys(self: &Arc<Self>) -> impl Stream<Item = Result<Key<'_>>> + Send {
	seek_stream::<stream::KeysRev<'_>, _>(self, Direction::Reverse, None)
}
