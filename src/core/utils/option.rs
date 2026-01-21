use futures::{FutureExt, Stream, future::OptionFuture};

use super::IterStream;

pub trait OptionExt<Fut, T, U>
where
	Fut: Future<Output = U> + Send,
	U: Send,
{
	fn map_async<F>(self, f: F) -> OptionFuture<Fut>
	where
		F: FnOnce(T) -> Fut;

	#[inline]
	fn map_stream<F>(self, f: F) -> impl Stream<Item = U> + Send
	where
		F: FnOnce(T) -> Fut,
		Self: Sized,
	{
		self.map_async(f)
			.map(Option::into_iter)
			.map(IterStream::stream)
			.flatten_stream()
	}
}

impl<Fut, T, U> OptionExt<Fut, T, U> for Option<T>
where
	Fut: Future<Output = U> + Send,
	U: Send,
{
	#[inline]
	fn map_async<F>(self, f: F) -> OptionFuture<Fut>
	where
		F: FnOnce(T) -> Fut,
	{
		self.map(f).into()
	}
}
