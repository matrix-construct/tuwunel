use futures::{FutureExt, Stream, future::OptionFuture};

use super::IterStream;

pub trait OptionExt<T> {
	fn map_async<F, Fut, U>(self, f: F) -> OptionFuture<Fut>
	where
		F: FnOnce(T) -> Fut,
		Fut: Future<Output = U> + Send,
		U: Send;

	#[inline]
	fn map_stream<F, Fut, U>(self, f: F) -> impl Stream<Item = U> + Send
	where
		F: FnOnce(T) -> Fut,
		Fut: Future<Output = U> + Send,
		U: Send,
		Self: Sized,
	{
		self.map_async(f)
			.map(Option::into_iter)
			.map(IterStream::stream)
			.flatten_stream()
	}
}

impl<T> OptionExt<T> for Option<T> {
	#[inline]
	fn map_async<F, Fut, U>(self, f: F) -> OptionFuture<Fut>
	where
		F: FnOnce(T) -> Fut,
		Fut: Future<Output = U> + Send,
		U: Send,
	{
		self.map(f).into()
	}
}
