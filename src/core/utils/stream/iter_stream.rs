use futures::{
	StreamExt, stream,
	stream::{Stream, TryStream},
};

use crate::{Error, Result};

pub trait IterStream<I: IntoIterator + Send> {
	/// Convert an Iterator into a Stream
	fn stream(self) -> impl Stream<Item = <I as IntoIterator>::Item> + Send;

	/// Convert an Iterator into a TryStream
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
	fn stream(self) -> impl Stream<Item = <I as IntoIterator>::Item> + Send {
		stream::iter(self)
	}

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
