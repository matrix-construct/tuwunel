//! Extended external extensions to futures::FutureExt

use futures::{future, future::Select};

/// Adds a stop-future race to ordinary futures.
///
/// The returned selector resolves when either future completes. Its output
/// identifies the winner and retains the unfinished future.
pub trait ExtExt<T>
where
	Self: Future<Output = T> + Send,
{
	/// Races the receiver against a unit-output stopping future.
	///
	/// `f` constructs the stopping future when the selector is created. The
	/// returned [`Select`] preserves whichever future has not completed.
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
