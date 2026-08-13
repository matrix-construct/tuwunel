use std::{fmt::Debug, sync::Arc};

use futures::stream::StreamExt;
use serde::Serialize;
use tuwunel_core::implement;

/// Counts items yielded by a complete forward key scan.
///
/// Each database entry normally contributes one item. Iterator failures are
/// counted as yielded items rather than returned to the caller.
#[implement(super::Map)]
#[inline]
pub fn count(self: &Arc<Self>) -> impl Future<Output = usize> + Send + '_ {
	self.raw_keys().count()
}

/// Counts forward scan items at or after a serialized lower bound.
///
/// The bound is encoded with the database serializer before seeking. Iterator
/// failures are counted as yielded items rather than returned to the caller.
///
/// # Panics
///
/// Panics if the lower bound cannot be serialized.
#[implement(super::Map)]
#[inline]
pub fn count_from<'a, P>(
	self: &'a Arc<Self>,
	from: &P,
) -> impl Future<Output = usize> + Send + 'a + use<'a, P>
where
	P: Serialize + ?Sized + Debug + 'a,
{
	self.keys_from_raw(from).count()
}

/// Counts forward scan items at or after a raw lower bound.
///
/// The supplied bytes are used directly as the seek key. Iterator failures are
/// counted as yielded items rather than returned to the caller.
#[implement(super::Map)]
#[inline]
pub fn raw_count_from<'a, P>(
	self: &'a Arc<Self>,
	from: &'a P,
) -> impl Future<Output = usize> + Send + 'a
where
	P: AsRef<[u8]> + ?Sized + Debug + Sync + 'a,
{
	self.raw_keys_from(from).count()
}

/// Counts forward scan items matching a serialized prefix.
///
/// The prefix is encoded with the database serializer before seeking. Iterator
/// failures are counted as yielded items rather than returned to the caller.
///
/// # Panics
///
/// Panics if the prefix cannot be serialized.
#[implement(super::Map)]
#[inline]
pub fn count_prefix<'a, P>(
	self: &'a Arc<Self>,
	prefix: &P,
) -> impl Future<Output = usize> + Send + 'a + use<'a, P>
where
	P: Serialize + ?Sized + Debug + 'a,
{
	self.keys_prefix_raw(prefix).count()
}

/// Counts forward scan items matching a raw prefix.
///
/// The supplied bytes are used directly for the seek and prefix test. Iterator
/// failures are counted as yielded items rather than returned to the caller.
#[implement(super::Map)]
#[inline]
pub fn raw_count_prefix<'a, P>(
	self: &'a Arc<Self>,
	prefix: &'a P,
) -> impl Future<Output = usize> + Send + 'a
where
	P: AsRef<[u8]> + ?Sized + Debug + Sync + 'a,
{
	self.raw_keys_prefix(prefix).count()
}
