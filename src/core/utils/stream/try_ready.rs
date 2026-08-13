//! Synchronous combinator extensions to futures::TryStream
#![expect(clippy::type_complexity)]

use futures::{
	future::{Ready, ready},
	stream::{
		AndThen, TryFilter, TryFilterMap, TryFold, TryForEach, TrySkipWhile, TryStream,
		TryStreamExt, TryTakeWhile,
	},
};

use crate::Result;

/// Adds synchronous combinators to fallible streams.
///
/// Closures run immediately when a successful item is polled, avoiding an async
/// block around non-async work. Source errors retain the stream's error type.
pub trait TryReadyExt<T, E, S>
where
	S: TryStream<Ok = T, Error = E, Item = Result<T, E>> + ?Sized,
	Self: TryStream + Sized,
{
	/// Applies a synchronous fallible transform to successful items.
	///
	/// Existing source errors bypass `f`. Successful transformed values and
	/// closure errors remain in source order.
	fn ready_and_then<U, F>(
		self,
		f: F,
	) -> AndThen<Self, Ready<Result<U, E>>, impl FnMut(S::Ok) -> Ready<Result<U, E>>>
	where
		F: Fn(S::Ok) -> Result<U, E>;

	/// Retains successful items accepted by a synchronous predicate.
	///
	/// The predicate borrows only successful values. Source errors pass through
	/// without invoking it.
	fn ready_try_filter<F>(
		self,
		f: F,
	) -> TryFilter<Self, Ready<bool>, impl FnMut(&S::Ok) -> Ready<bool>>
	where
		F: Fn(&S::Ok) -> bool;

	/// Maps successful items through a synchronous fallible filter.
	///
	/// `Ok(Some(value))` yields a value and `Ok(None)` discards the item.
	/// Source errors and closure errors remain in the result stream.
	fn ready_try_filter_map<F, U>(
		self,
		f: F,
	) -> TryFilterMap<
		Self,
		Ready<Result<Option<U>, E>>,
		impl FnMut(S::Ok) -> Ready<Result<Option<U>, E>>,
	>
	where
		F: Fn(S::Ok) -> Result<Option<U>, E>;

	/// Folds successful items with a synchronous fallible accumulator.
	///
	/// Folding begins from `init` and processes values in source order. A
	/// source or closure error ends the returned future with that error.
	fn ready_try_fold<U, F>(
		self,
		init: U,
		f: F,
	) -> TryFold<Self, Ready<Result<U, E>>, U, impl FnMut(U, S::Ok) -> Ready<Result<U, E>>>
	where
		F: Fn(U, S::Ok) -> Result<U, E>;

	/// Folds successful items from `U::default()` with a fallible accumulator.
	///
	/// Values are processed in source order. An empty stream returns the
	/// default, while a source or closure error ends the fold.
	fn ready_try_fold_default<U, F>(
		self,
		f: F,
	) -> TryFold<Self, Ready<Result<U, E>>, U, impl FnMut(U, S::Ok) -> Ready<Result<U, E>>>
	where
		F: Fn(U, S::Ok) -> Result<U, E>,
		U: Default;

	/// Applies a synchronous fallible closure to each successful item.
	///
	/// Values are processed in source order. A source or closure error ends the
	/// returned future before later items are visited.
	fn ready_try_for_each<F>(
		self,
		f: F,
	) -> TryForEach<Self, Ready<Result<(), E>>, impl FnMut(S::Ok) -> Ready<Result<(), E>>>
	where
		F: FnMut(S::Ok) -> Result<(), E>;

	/// Skips leading successful items accepted by a fallible predicate.
	///
	/// The first false item and later items are yielded without further
	/// predicate calls. Source and predicate errors remain in the result
	/// stream.
	fn ready_try_skip_while<F>(
		self,
		f: F,
	) -> TrySkipWhile<Self, Ready<Result<bool, E>>, impl FnMut(&S::Ok) -> Ready<Result<bool, E>>>
	where
		F: Fn(&S::Ok) -> Result<bool, E>;

	/// Yields leading successful items accepted by a fallible predicate.
	///
	/// The first false item ends the stream and is not yielded. Source and
	/// predicate errors are yielded without terminating the adapter, and a
	/// predicate error discards the tested item.
	fn ready_try_take_while<F>(
		self,
		f: F,
	) -> TryTakeWhile<Self, Ready<Result<bool, E>>, impl FnMut(&S::Ok) -> Ready<Result<bool, E>>>
	where
		F: Fn(&S::Ok) -> Result<bool, E>;
}

impl<T, E, S> TryReadyExt<T, E, S> for S
where
	S: TryStream<Ok = T, Error = E, Item = Result<T, E>> + ?Sized,
	Self: TryStream + Sized,
{
	#[inline]
	fn ready_and_then<U, F>(
		self,
		f: F,
	) -> AndThen<Self, Ready<Result<U, E>>, impl FnMut(S::Ok) -> Ready<Result<U, E>>>
	where
		F: Fn(S::Ok) -> Result<U, E>,
	{
		self.and_then(move |t| ready(f(t)))
	}

	#[inline]
	fn ready_try_filter<F>(
		self,
		f: F,
	) -> TryFilter<Self, Ready<bool>, impl FnMut(&S::Ok) -> Ready<bool>>
	where
		F: Fn(&S::Ok) -> bool,
	{
		self.try_filter(move |t| ready(f(t)))
	}

	#[inline]
	fn ready_try_filter_map<F, U>(
		self,
		f: F,
	) -> TryFilterMap<
		Self,
		Ready<Result<Option<U>, E>>,
		impl FnMut(S::Ok) -> Ready<Result<Option<U>, E>>,
	>
	where
		F: Fn(S::Ok) -> Result<Option<U>, E>,
	{
		self.try_filter_map(move |t| ready(f(t)))
	}

	#[inline]
	fn ready_try_fold<U, F>(
		self,
		init: U,
		f: F,
	) -> TryFold<Self, Ready<Result<U, E>>, U, impl FnMut(U, S::Ok) -> Ready<Result<U, E>>>
	where
		F: Fn(U, S::Ok) -> Result<U, E>,
	{
		self.try_fold(init, move |a, t| ready(f(a, t)))
	}

	#[inline]
	fn ready_try_fold_default<U, F>(
		self,
		f: F,
	) -> TryFold<Self, Ready<Result<U, E>>, U, impl FnMut(U, S::Ok) -> Ready<Result<U, E>>>
	where
		F: Fn(U, S::Ok) -> Result<U, E>,
		U: Default,
	{
		self.ready_try_fold(U::default(), f)
	}

	#[inline]
	fn ready_try_for_each<F>(
		self,
		mut f: F,
	) -> TryForEach<Self, Ready<Result<(), E>>, impl FnMut(S::Ok) -> Ready<Result<(), E>>>
	where
		F: FnMut(S::Ok) -> Result<(), E>,
	{
		self.try_for_each(move |t| ready(f(t)))
	}

	#[inline]
	fn ready_try_skip_while<F>(
		self,
		f: F,
	) -> TrySkipWhile<Self, Ready<Result<bool, E>>, impl FnMut(&S::Ok) -> Ready<Result<bool, E>>>
	where
		F: Fn(&S::Ok) -> Result<bool, E>,
	{
		self.try_skip_while(move |t| ready(f(t)))
	}

	#[inline]
	fn ready_try_take_while<F>(
		self,
		f: F,
	) -> TryTakeWhile<Self, Ready<Result<bool, E>>, impl FnMut(&S::Ok) -> Ready<Result<bool, E>>>
	where
		F: Fn(&S::Ok) -> Result<bool, E>,
	{
		self.try_take_while(move |t| ready(f(t)))
	}
}
