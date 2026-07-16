//! Extended external extensions to futures::FutureExt

use futures::{future, future::Select};

/// This interface is not necessarily complete; feel free to add as-needed.
pub trait ExtExt<T>
where
	Self: Future<Output = T> + Send,
{
	fn until<A, B, F>(self, f: F) -> Select<A, B>
	where
		Self: Sized,
		F: FnOnce() -> B,
		A: Future<Output = T> + From<Self> + Send + Unpin,
		B: Future<Output = ()> + Send + Unpin;
}

impl<T, Fut> ExtExt<T> for Fut
where
	Fut: Future<Output = T> + Send,
{
	#[inline]
	fn until<A, B, F>(self, f: F) -> Select<A, B>
	where
		Self: Sized,
		F: FnOnce() -> B,
		A: Future<Output = T> + From<Self> + Send + Unpin,
		B: Future<Output = ()> + Send + Unpin,
	{
		future::select(self.into(), f())
	}
}
