//! Synchronous combinator extensions to futures::TryStream

use futures::{TryFuture, TryStream, TryStreamExt, future::ready};

use super::automatic_width;
use crate::Result;

/// Adds bounded concurrent transformations with completion-ordered outputs.
///
/// Successful item futures may run ahead of downstream demand, and their
/// results are yielded as they complete. Source errors bypass the transform and
/// may overtake queued item futures.
pub trait TryBroadbandExt<T, E>
where
	Self: TryStream<Ok = T, Error = E, Item = Result<T, E>> + Send + Sized,
{
	/// Tests all successful items concurrently with an explicit width.
	///
	/// `n` limits in-flight predicates, `None` selects the automatic width,
	/// and an explicit zero cannot make progress. The first observed false or
	/// source or predicate error short-circuits; source errors may overtake
	/// queued predicates. An empty stream resolves `Ok(true)`.
	fn broadn_try_all<F, Fut, N>(
		self,
		n: N,
		f: F,
	) -> impl Future<Output = Result<bool, E>> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Self::Ok) -> Fut + Send,
		Fut: TryFuture<Ok = bool, Error = E, Output = Result<bool, E>> + Send;

	/// Tests successful items concurrently for any match with an explicit width.
	///
	/// `n` limits in-flight predicates, `None` selects the automatic width,
	/// and an explicit zero cannot make progress. The first observed true or
	/// source or predicate error short-circuits; source errors may overtake
	/// queued predicates. An empty stream resolves `Ok(false)`.
	fn broadn_try_any<F, Fut, N>(
		self,
		n: N,
		f: F,
	) -> impl Future<Output = Result<bool, E>> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Self::Ok) -> Fut + Send,
		Fut: TryFuture<Ok = bool, Error = E, Output = Result<bool, E>> + Send;

	/// Transforms successful items concurrently with an explicit width.
	///
	/// `n` limits in-flight item futures, while `None` selects the automatic
	/// width; an explicit zero cannot make progress. Transformation results are
	/// completion ordered, while source errors bypass `f` and may overtake
	/// them.
	fn broadn_and_then<U, F, Fut, N>(
		self,
		n: N,
		f: F,
	) -> impl TryStream<Ok = U, Error = E, Item = Result<U, E>> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Self::Ok) -> Fut + Send,
		Fut: TryFuture<Ok = U, Error = E, Output = Result<U, E>> + Send;

	/// Tests all successful items concurrently with the automatic width.
	///
	/// The first observed false or source or predicate error short-circuits,
	/// so a false result can precede a pending error. An empty stream resolves
	/// `Ok(true)`.
	fn broad_try_all<F, Fut>(self, f: F) -> impl Future<Output = Result<bool, E>> + Send
	where
		F: Fn(Self::Ok) -> Fut + Send,
		Fut: TryFuture<Ok = bool, Error = E, Output = Result<bool, E>> + Send,
	{
		self.broadn_try_all(None, f)
	}

	/// Tests successful items concurrently for any match with the automatic width.
	///
	/// The first observed true or source or predicate error short-circuits,
	/// so a true result can precede a pending error. An empty stream resolves
	/// `Ok(false)`.
	fn broad_try_any<F, Fut>(self, f: F) -> impl Future<Output = Result<bool, E>> + Send
	where
		F: Fn(Self::Ok) -> Fut + Send,
		Fut: TryFuture<Ok = bool, Error = E, Output = Result<bool, E>> + Send,
	{
		self.broadn_try_any(None, f)
	}

	/// Transforms successful items concurrently with the automatic width.
	///
	/// Transformation results are yielded in completion order rather than
	/// source order. Existing source errors bypass `f` and may overtake queued
	/// futures.
	fn broad_and_then<U, F, Fut>(
		self,
		f: F,
	) -> impl TryStream<Ok = U, Error = E, Item = Result<U, E>> + Send
	where
		F: Fn(Self::Ok) -> Fut + Send,
		Fut: TryFuture<Ok = U, Error = E, Output = Result<U, E>> + Send,
	{
		self.broadn_and_then(None, f)
	}
}

impl<T, E, S> TryBroadbandExt<T, E> for S
where
	S: TryStream<Ok = T, Error = E, Item = Result<T, E>> + Send + Sized,
{
	fn broadn_try_all<F, Fut, N>(self, n: N, f: F) -> impl Future<Output = Result<bool, E>> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Self::Ok) -> Fut + Send,
		Fut: TryFuture<Ok = bool, Error = E, Output = Result<bool, E>> + Send,
	{
		self.broadn_and_then(n, f).try_all(ready)
	}

	fn broadn_try_any<F, Fut, N>(self, n: N, f: F) -> impl Future<Output = Result<bool, E>> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Self::Ok) -> Fut + Send,
		Fut: TryFuture<Ok = bool, Error = E, Output = Result<bool, E>> + Send,
	{
		self.broadn_and_then(n, f).try_any(ready)
	}

	fn broadn_and_then<U, F, Fut, N>(
		self,
		n: N,
		f: F,
	) -> impl TryStream<Ok = U, Error = E, Item = Result<U, E>> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Self::Ok) -> Fut + Send,
		Fut: TryFuture<Ok = U, Error = E, Output = Result<U, E>> + Send,
	{
		self.map_ok(f)
			.try_buffer_unordered(n.into().unwrap_or_else(automatic_width))
	}
}
