//! Extended external extensions to futures::FutureExt

use std::time::{Duration, Instant};

use futures::{
	FutureExt, future,
	future::{Select, lazy},
};

/// Adds a stop-future race, a completion timer and a debug-assertion box to
/// ordinary futures.
///
/// `until` hands back a selector over the receiver and a stopping future;
/// `timed` hands back the receiver's output with how long it took;
/// `debug_boxed` boxes the receiver only while debug assertions are on.
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

	/// Boxes the receiver while debug assertions are on and returns it as is
	/// otherwise.
	///
	/// An unoptimized build keeps each awaited future's temporaries in its
	/// callers' poll frames, so a deep path can outgrow the worker stack there.
	/// With debug assertions on, the caller holds a pointer to the boxed child
	/// instead of its whole state; without them there is no allocation.
	fn debug_boxed<'a>(self) -> impl Future<Output = T> + Send + 'a
	where
		Self: Sized + 'a,
		T: 'a;
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

	#[inline]
	fn debug_boxed<'a>(self) -> impl Future<Output = T> + Send + 'a
	where
		Self: Sized + 'a,
		T: 'a,
	{
		cfg_select! {
			debug_assertions => self.boxed(), // size firewall, type-erasure boundary
			_ => self,
		}
	}
}
