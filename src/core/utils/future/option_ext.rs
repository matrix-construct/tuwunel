#![expect(clippy::wrong_self_convention)]

use futures::{
	FutureExt,
	future::{OptionFuture, ready},
};

/// Adds option-like combinators to futures with optional output.
///
/// Each adapter transforms the eventual `Option<T>` without requiring an
/// intermediate await. Lazy fallbacks run only for absent output.
pub trait OptionFutureExt<T> {
	/// Tests whether the future yields no value or one matching `f`.
	///
	/// An absent output returns true without invoking the predicate. A present
	/// value is borrowed for the predicate after the future resolves.
	fn is_none_or(self, f: impl FnOnce(&T) -> bool + Send) -> impl Future<Output = bool> + Send;

	/// Tests whether the future yields a value matching `f`.
	///
	/// An absent output returns false without invoking the predicate. A present
	/// value is borrowed for the predicate after the future resolves.
	fn is_some_and(self, f: impl FnOnce(&T) -> bool + Send) -> impl Future<Output = bool> + Send;

	/// Returns the future's value or the supplied fallback.
	///
	/// The fallback is supplied eagerly and used only for absent output. A
	/// present value passes through unchanged.
	fn unwrap_or(self, t: T) -> impl Future<Output = T> + Send;

	/// Returns the future's value or `T::default()`.
	///
	/// The default is constructed only after the future yields `None`. A
	/// present value passes through unchanged.
	fn unwrap_or_default(self) -> impl Future<Output = T> + Send
	where
		T: Default;

	/// Returns the future's value or lazily computes a fallback.
	///
	/// The closure is called only after the future yields `None`. A present
	/// value passes through without invoking it.
	fn unwrap_or_else(self, f: impl FnOnce() -> T + Send) -> impl Future<Output = T> + Send;

	/// Returns the future's value or lazily awaits an asynchronous fallback.
	///
	/// The closure is called only after the future yields `None`. A present
	/// value passes through without invoking it.
	fn unwrap_or_else_async<F: Future<Output = T> + Send>(
		self,
		f: impl FnOnce() -> F + Send,
	) -> impl Future<Output = T> + Send;
}

impl<T, Fut> OptionFutureExt<T> for OptionFuture<Fut>
where
	Fut: Future<Output = T> + Send,
	T: Send,
{
	#[inline]
	fn is_none_or(self, f: impl FnOnce(&T) -> bool + Send) -> impl Future<Output = bool> + Send {
		self.map(|o| o.as_ref().is_none_or(f))
	}

	#[inline]
	fn is_some_and(self, f: impl FnOnce(&T) -> bool + Send) -> impl Future<Output = bool> + Send {
		self.map(|o| o.as_ref().is_some_and(f))
	}

	#[inline]
	fn unwrap_or(self, t: T) -> impl Future<Output = T> + Send { self.map(|o| o.unwrap_or(t)) }

	#[inline]
	fn unwrap_or_default(self) -> impl Future<Output = T> + Send
	where
		T: Default,
	{
		self.map(Option::unwrap_or_default)
	}

	#[inline]
	fn unwrap_or_else(self, f: impl FnOnce() -> T + Send) -> impl Future<Output = T> + Send {
		self.map(|o| o.unwrap_or_else(f))
	}

	#[inline]
	fn unwrap_or_else_async<F: Future<Output = T> + Send>(
		self,
		f: impl FnOnce() -> F + Send,
	) -> impl Future<Output = T> + Send {
		self.map(|o| o.map_or_else(|| f().right_future(), |t| ready(t).left_future()))
			.flatten()
	}
}
