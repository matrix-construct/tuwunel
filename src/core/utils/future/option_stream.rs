use futures::{FutureExt, Stream, StreamExt, future::OptionFuture};

use super::super::IterStream;

pub trait OptionStream<T> {
	fn stream(self) -> impl Stream<Item = T> + Send;
}

impl<T, O, S, Fut> OptionStream<T> for OptionFuture<Fut>
where
	Fut: Future<Output = (O, S)> + Send,
	S: Stream<Item = T> + Send,
	O: IntoIterator<Item = T> + Send,
	<O as IntoIterator>::IntoIter: Send,
	T: Send,
{
	#[inline]
	fn stream(self) -> impl Stream<Item = T> + Send {
		self.map(|opt| opt.map(|(curr, next)| curr.into_iter().stream().chain(next)))
			.map(Option::into_iter)
			.map(IterStream::stream)
			.flatten_stream()
			.flatten()
	}
}
