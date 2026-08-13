//! Synchronous combinator extensions to futures::TryStream

use futures::{TryFuture, TryStream, TryStreamExt};

use super::automatic_width;
use crate::Result;

/// Adds bounded concurrent transformations with ordered transform results.
///
/// Successful item futures may run ahead of downstream demand, and their
/// results retain queue order. Source errors are propagated immediately and may
/// overtake queued transformation futures.
pub trait TryWidebandExt<T, E>
where
	Self: TryStream<Ok = T, Error = E, Item = Result<T, E>> + Send + Sized,
{
	/// Transforms successful items concurrently with an explicit width.
	///
	/// `n` limits in-flight item futures, while `None` selects the automatic
	/// width; an explicit zero cannot make progress. Transformation results
	/// retain queue order, while source errors may overtake them.
	fn widen_and_then<U, F, Fut, N>(
		self,
		n: N,
		f: F,
	) -> impl TryStream<Ok = U, Error = E, Item = Result<U, E>> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Self::Ok) -> Fut + Send,
		Fut: TryFuture<Ok = U, Error = E, Output = Result<U, E>> + Send,
		U: Send;

	/// Transforms successful items concurrently with the automatic width.
	///
	/// Item futures may run ahead, but transformation results retain queue
	/// order. Existing source errors bypass `f` and may overtake queued
	/// futures.
	fn wide_and_then<U, F, Fut>(
		self,
		f: F,
	) -> impl TryStream<Ok = U, Error = E, Item = Result<U, E>> + Send
	where
		F: Fn(Self::Ok) -> Fut + Send,
		Fut: TryFuture<Ok = U, Error = E, Output = Result<U, E>> + Send,
		U: Send,
	{
		self.widen_and_then(None, f)
	}
}

impl<T, E, S> TryWidebandExt<T, E> for S
where
	S: TryStream<Ok = T, Error = E, Item = Result<T, E>> + Send + Sized,
	E: Send,
{
	fn widen_and_then<U, F, Fut, N>(
		self,
		n: N,
		f: F,
	) -> impl TryStream<Ok = U, Error = E, Item = Result<U, E>> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Self::Ok) -> Fut + Send,
		Fut: TryFuture<Ok = U, Error = E, Output = Result<U, E>> + Send,
		U: Send,
	{
		self.map_ok(f)
			.try_buffered(n.into().unwrap_or_else(automatic_width))
	}
}
