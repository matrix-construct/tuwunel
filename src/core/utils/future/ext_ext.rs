//! Extended external extensions to futures::FutureExt

use std::time::{Duration, Instant};

use futures::{
	FutureExt, future,
	future::{Select, lazy},
};

/// Adds a stop-future race and a completion timer to ordinary futures.
///
/// `until` hands back a selector over the receiver and a stopping future;
/// `timed` hands back the receiver's output with how long it took.
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

	/// Resolves to the receiver's output and how long it took.
	///
	/// The clock starts when the returned future is first polled, so time spent
	/// before it is awaited does not count.
	fn timed(self) -> impl Future<Output = (T, Duration)> + Send
	where
		Self: Sized;
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

	#[inline]
	fn timed(self) -> impl Future<Output = (T, Duration)> + Send
	where
		Self: Sized,
	{
		lazy(|_| Instant::now())
			.then(move |started| self.map(move |output| (output, started.elapsed())))
	}
}
