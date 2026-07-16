use std::{fmt::Debug, sync::Arc};

use futures::{Stream, StreamExt, TryStreamExt, future};
use serde::{Deserialize, Serialize};
use tuwunel_core::{Result, implement};

use crate::keyval::{Key, result_deserialize_key, serialize_key};

#[implement(super::Map)]
pub fn rev_keys_prefix<'a, K, P>(
	self: &'a Arc<Self>,
	prefix: &P,
) -> impl Stream<Item = Result<Key<'_, K>>> + Send + use<'a, K, P>
where
	P: Serialize + ?Sized + Debug,
	K: Deserialize<'a> + Send,
{
	self.rev_keys_prefix_raw(prefix)
		.map(result_deserialize_key::<K>)
}

#[implement(super::Map)]
#[tracing::instrument(skip(self), level = "trace")]
pub fn rev_keys_prefix_raw<P>(
	self: &Arc<Self>,
	prefix: &P,
) -> impl Stream<Item = Result<Key<'_>>> + Send + use<'_, P>
where
	P: Serialize + ?Sized + Debug,
{
	let key = serialize_key(prefix).expect("failed to serialize query key");
	self.rev_raw_keys_from(&key)
		.try_take_while(move |k: &Key<'_>| future::ok(k.starts_with(&key)))
}

#[implement(super::Map)]
pub fn rev_keys_raw_prefix<'a, K, P>(
	self: &'a Arc<Self>,
	prefix: &'a P,
) -> impl Stream<Item = Result<Key<'_, K>>> + Send + 'a
where
	P: AsRef<[u8]> + ?Sized + Debug + Sync + 'a,
	K: Deserialize<'a> + Send + 'a,
{
	self.rev_raw_keys_prefix(prefix)
		.map(result_deserialize_key::<K>)
}

#[implement(super::Map)]
pub fn rev_raw_keys_prefix<'a, P>(
	self: &'a Arc<Self>,
	prefix: &'a P,
) -> impl Stream<Item = Result<Key<'_>>> + Send + 'a
where
	P: AsRef<[u8]> + ?Sized + Debug + Sync + 'a,
{
	self.rev_raw_keys_from(prefix)
		.try_take_while(|k: &Key<'_>| future::ok(k.starts_with(prefix.as_ref())))
}
