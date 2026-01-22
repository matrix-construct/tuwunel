//! Synchronous combinator extensions to futures::TryStream
#![expect(clippy::type_complexity)]

use futures::{
	future::{Ready, ready},
	stream::{
		AndThen, TryFilterMap, TryFold, TryForEach, TrySkipWhile, TryStream, TryStreamExt,
		TryTakeWhile,
	},
};

use crate::Result;

/// Synchronous combinators to augment futures::TryStreamExt.
///
/// This interface is not necessarily complete; feel free to add as-needed.
pub trait TryReadyExt<T, E, S>
where
	S: TryStream<Ok = T, Error = E, Item = Result<T, E>> + ?Sized,
	Self: TryStream + Sized,
{
	fn ready_and_then<U, F>(
		self,
		f: F,
	) -> AndThen<Self, Ready<Result<U, E>>, impl FnMut(S::Ok) -> Ready<Result<U, E>>>
	where
		F: Fn(S::Ok) -> Result<U, E>;

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

	fn ready_try_fold<U, F>(
		self,
		init: U,
		f: F,
	) -> TryFold<Self, Ready<Result<U, E>>, U, impl FnMut(U, S::Ok) -> Ready<Result<U, E>>>
	where
		F: Fn(U, S::Ok) -> Result<U, E>;

	fn ready_try_fold_default<U, F>(
		self,
		f: F,
	) -> TryFold<Self, Ready<Result<U, E>>, U, impl FnMut(U, S::Ok) -> Ready<Result<U, E>>>
	where
		F: Fn(U, S::Ok) -> Result<U, E>,
		U: Default;

	fn ready_try_for_each<F>(
		self,
		f: F,
	) -> TryForEach<Self, Ready<Result<(), E>>, impl FnMut(S::Ok) -> Ready<Result<(), E>>>
	where
		F: FnMut(S::Ok) -> Result<(), E>;

	fn ready_try_skip_while<F>(
		self,
		f: F,
	) -> TrySkipWhile<Self, Ready<Result<bool, E>>, impl FnMut(&S::Ok) -> Ready<Result<bool, E>>>
	where
		F: Fn(&S::Ok) -> Result<bool, E>;

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
