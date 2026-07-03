use std::sync::Arc;

use futures::{Stream, StreamExt};
use rocksdb::Direction;
use serde::Deserialize;
use tuwunel_core::{Result, implement};

use super::seek::seek_stream;
use crate::{keyval, keyval::KeyVal, stream};

/// Iterate key-value entries in the map from the beginning.
///
/// - Result is deserialized
#[implement(super::Map)]
pub fn stream<'a, K, V>(
	self: &'a Arc<Self>,
) -> impl Stream<Item = Result<KeyVal<'_, K, V>>> + Send
where
	K: Deserialize<'a> + Send,
	V: Deserialize<'a> + Send,
{
	self.raw_stream()
		.map(keyval::result_deserialize::<K, V>)
}

/// Iterate key-value entries in the map from the beginning.
///
/// - Result is raw
#[implement(super::Map)]
#[tracing::instrument(skip(self), fields(%self), level = "trace")]
pub fn raw_stream(self: &Arc<Self>) -> impl Stream<Item = Result<KeyVal<'_>>> + Send {
	seek_stream::<stream::Items<'_>, _>(self, Direction::Forward, None)
}
