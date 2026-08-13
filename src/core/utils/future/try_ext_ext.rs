//! Extended external extensions to futures::TryFutureExt
#![expect(clippy::type_complexity)]
// is_ok() has to consume *self rather than borrow. This extension is for a
// caller only ever caring about result status while discarding all contents.
#![expect(clippy::wrong_self_convention)]

use futures::{
	TryFuture, TryFutureExt, future,
	future::{MapOkOrElse, TrySelect, UnwrapOrElse},
};

/// Adds result-like combinators to fallible futures.
///
/// The adapters transform the eventual success or error without awaiting the
/// future early. Inspection helpers reduce either result branch to a Boolean
/// and discard its payload.
pub trait TryExtExt<T, E>
where
	Self: TryFuture<Ok = T, Error = E> + Send,
{
	/// Reports whether the future resolves to an error.
	///
	/// Both output payloads are discarded after the future completes. The
	/// returned future preserves only the error-state Boolean.
	fn is_err(
		self,
	) -> MapOkOrElse<Self, impl FnOnce(Self::Ok) -> bool, impl FnOnce(Self::Error) -> bool>
	where
		Self: Sized;

	/// Reports whether the future resolves successfully.
	///
	/// Both output payloads are discarded after the future completes. The
	/// returned future preserves only the success-state Boolean.
	#[expect(clippy::wrong_self_convention)]
	fn is_ok(
		self,
	) -> MapOkOrElse<Self, impl FnOnce(Self::Ok) -> bool, impl FnOnce(Self::Error) -> bool>
	where
		Self: Sized;

	/// Maps a successful output or returns an eager fallback for errors.
	///
	/// The mapping closure runs only for the success branch. The original error
	/// is discarded, and the fallback is dropped when mapping succeeds.
	fn map_ok_or<U, F>(
		self,
		default: U,
		f: F,
	) -> MapOkOrElse<Self, impl FnOnce(Self::Ok) -> U, impl FnOnce(Self::Error) -> U>
	where
		F: FnOnce(Self::Ok) -> U,
		Self: Send + Sized;

	/// Converts the fallible future's output into an option.
	///
	/// A successful value becomes `Some`, while any error becomes `None`. The
	/// original error value is discarded.
	fn ok(
		self,
	) -> MapOkOrElse<
		Self,
		impl FnOnce(Self::Ok) -> Option<Self::Ok>,
		impl FnOnce(Self::Error) -> Option<Self::Ok>,
	>
	where
		Self: Sized;

	/// Races the receiver against a fallible unit-output stopping future.
	///
	/// `f` constructs the stopping future when the selector is created. The
	/// returned [`TrySelect`] preserves the winning result and unfinished
	/// future.
	fn try_until<A, B, F>(self, f: F) -> TrySelect<A, B>
	where
		Self: Sized,
		F: FnOnce() -> B,
		A: TryFuture<Ok = Self::Ok> + From<Self> + Send + Unpin,
		B: TryFuture<Ok = (), Error = Self::Error> + Send + Unpin;

	/// Returns a successful output or an eager fallback for errors.
	///
	/// The original error is discarded after the future resolves. The fallback
	/// is dropped when the future succeeds.
	fn unwrap_or(
		self,
		default: Self::Ok,
	) -> UnwrapOrElse<Self, impl FnOnce(Self::Error) -> Self::Ok>
	where
		Self: Sized;

	/// Returns a successful output or its type's default for errors.
	///
	/// The default is constructed eagerly when the adapter is created and is
	/// dropped when the future succeeds. The original error value is discarded.
	fn unwrap_or_default(self) -> UnwrapOrElse<Self, impl FnOnce(Self::Error) -> Self::Ok>
	where
		Self: Sized,
		Self::Ok: Default;
}

impl<T, E, Fut> TryExtExt<T, E> for Fut
where
	Fut: TryFuture<Ok = T, Error = E> + Send,
{
	#[inline]
	fn is_err(
		self,
	) -> MapOkOrElse<Self, impl FnOnce(Self::Ok) -> bool, impl FnOnce(Self::Error) -> bool>
	where
		Self: Sized,
	{
		self.map_ok_or(true, |_| false)
	}

	#[inline]
	fn is_ok(
		self,
	) -> MapOkOrElse<Self, impl FnOnce(Self::Ok) -> bool, impl FnOnce(Self::Error) -> bool>
	where
		Self: Sized,
	{
		self.map_ok_or(false, |_| true)
	}

	#[inline]
	fn map_ok_or<U, F>(
		self,
		default: U,
		f: F,
	) -> MapOkOrElse<Self, impl FnOnce(Self::Ok) -> U, impl FnOnce(Self::Error) -> U>
	where
		F: FnOnce(Self::Ok) -> U,
		Self: Send + Sized,
	{
		self.map_ok_or_else(|_| default, f)
	}

	#[inline]
	fn ok(
		self,
	) -> MapOkOrElse<
		Self,
		impl FnOnce(Self::Ok) -> Option<Self::Ok>,
		impl FnOnce(Self::Error) -> Option<Self::Ok>,
	>
	where
		Self: Sized,
	{
		self.map_ok_or(None, Some)
	}

	#[inline]
	fn try_until<A, B, F>(self, f: F) -> TrySelect<A, B>
	where
		Self: Sized,
		F: FnOnce() -> B,
		A: TryFuture<Ok = Self::Ok> + From<Self> + Send + Unpin,
		B: TryFuture<Ok = (), Error = Self::Error> + Send + Unpin,
	{
		future::try_select(self.into(), f())
	}

	#[inline]
	fn unwrap_or(
		self,
		default: Self::Ok,
	) -> UnwrapOrElse<Self, impl FnOnce(Self::Error) -> Self::Ok>
	where
		Self: Sized,
	{
		self.unwrap_or_else(move |_| default)
	}

	#[inline]
	fn unwrap_or_default(self) -> UnwrapOrElse<Self, impl FnOnce(Self::Error) -> Self::Ok>
	where
		Self: Sized,
		Self::Ok: Default,
	{
		self.unwrap_or(Default::default())
	}
}
