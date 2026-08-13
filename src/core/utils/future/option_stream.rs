use futures::{FutureExt, Stream, StreamExt, future::OptionFuture};

use super::super::IterStream;

/// Converts an optional future of initial and trailing items into a stream.
///
/// A present future yields its iterable items before chaining its trailing
/// stream. An absent future becomes an empty stream.
pub trait OptionStream<T> {
	/// Flattens the optional future into one ordered stream.
	///
	/// Items from the returned iterable are emitted before the accompanying
	/// stream is polled. No items are emitted when the optional future is
	/// absent.
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
