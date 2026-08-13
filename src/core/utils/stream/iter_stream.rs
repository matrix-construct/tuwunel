use futures::{
	StreamExt, stream,
	stream::{Stream, TryStream},
};

use crate::{Error, Result};

/// Converts synchronous iterables into immediately ready streams.
///
/// Source iteration order is preserved. The fallible form wraps each item in a
/// successful result using the crate's error type.
pub trait IterStream<I: IntoIterator + Send> {
	/// Converts the iterable into a stream of its items.
	///
	/// Items are yielded in the source iterator's order. Polling requires no
	/// asynchronous work beyond advancing that iterator.
	fn stream(self) -> impl Stream<Item = <I as IntoIterator>::Item> + Send;

	/// Converts the iterable into a stream of successful results.
	///
	/// Every source item is wrapped in `Ok` with [`Error`] as the error type.
	/// The adapter itself never produces an error.
	fn try_stream(
		self,
	) -> impl TryStream<
		Ok = <I as IntoIterator>::Item,
		Error = Error,
		Item = Result<<I as IntoIterator>::Item, Error>,
	> + Send;
}

impl<I> IterStream<I> for I
where
	I: IntoIterator + Send,
	<I as IntoIterator>::IntoIter: Send,
{
	#[inline]
	fn stream(self) -> impl Stream<Item = <I as IntoIterator>::Item> + Send { stream::iter(self) }

	#[inline]
	fn try_stream(
		self,
	) -> impl TryStream<
		Ok = <I as IntoIterator>::Item,
		Error = Error,
		Item = Result<<I as IntoIterator>::Item, Error>,
	> + Send {
		self.stream().map(Ok)
	}
}
