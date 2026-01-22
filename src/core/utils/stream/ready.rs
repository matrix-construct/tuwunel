//! Synchronous combinator extensions to futures::Stream
#![expect(clippy::type_complexity)]

use futures::{
	future::{FutureExt, Ready, ready},
	stream::{
		All, Any, Filter, FilterMap, Fold, ForEach, Scan, SkipWhile, Stream, StreamExt, TakeWhile,
	},
};

/// Synchronous combinators to augment futures::StreamExt. Most Stream
/// combinators take asynchronous arguments, but often only simple predicates
/// are required to steer a Stream like an Iterator. This suite provides a
/// convenience to reduce boilerplate by de-cluttering non-async predicates.
///
/// This interface is not necessarily complete; feel free to add as-needed.
pub trait ReadyExt<Item>
where
	Self: Stream<Item = Item> + Sized,
{
	fn ready_all<F>(self, f: F) -> All<Self, Ready<bool>, impl FnMut(Item) -> Ready<bool>>
	where
		F: Fn(Item) -> bool;

	fn ready_any<F>(self, f: F) -> Any<Self, Ready<bool>, impl FnMut(Item) -> Ready<bool>>
	where
		F: Fn(Item) -> bool;

	fn ready_find<'a, F>(self, f: F) -> impl Future<Output = Option<Item>> + Send
	where
		Self: Send + Unpin + 'a,
		F: Fn(&Item) -> bool + Send + 'a,
		Item: Send;

	fn ready_find_map<'a, F, U>(self, f: F) -> impl Future<Output = Option<U>> + Send
	where
		Self: Send + Unpin + 'a,
		F: Fn(Item) -> Option<U> + Send + 'a,
		Item: Send,
		U: Send;

	fn ready_filter<'a, F>(
		self,
		f: F,
	) -> Filter<Self, Ready<bool>, impl FnMut(&Item) -> Ready<bool> + 'a>
	where
		F: Fn(&Item) -> bool + 'a;

	fn ready_filter_map<F, U>(
		self,
		f: F,
	) -> FilterMap<Self, Ready<Option<U>>, impl FnMut(Item) -> Ready<Option<U>>>
	where
		F: Fn(Item) -> Option<U>;

	fn ready_fold<T, F>(
		self,
		init: T,
		f: F,
	) -> Fold<Self, Ready<T>, T, impl FnMut(T, Item) -> Ready<T>>
	where
		F: Fn(T, Item) -> T;

	fn ready_fold_default<T, F>(
		self,
		f: F,
	) -> Fold<Self, Ready<T>, T, impl FnMut(T, Item) -> Ready<T>>
	where
		F: Fn(T, Item) -> T,
		T: Default;

	fn ready_for_each<F>(self, f: F) -> ForEach<Self, Ready<()>, impl FnMut(Item) -> Ready<()>>
	where
		F: FnMut(Item);

	fn ready_take_while<'a, F>(
		self,
		f: F,
	) -> TakeWhile<Self, Ready<bool>, impl FnMut(&Item) -> Ready<bool> + 'a>
	where
		F: Fn(&Item) -> bool + 'a;

	fn ready_scan<B, T, F>(
		self,
		init: T,
		f: F,
	) -> Scan<Self, T, Ready<Option<B>>, impl FnMut(&mut T, Item) -> Ready<Option<B>>>
	where
		F: Fn(&mut T, Item) -> Option<B>;

	fn ready_scan_each<T, F>(
		self,
		init: T,
		f: F,
	) -> Scan<Self, T, Ready<Option<Item>>, impl FnMut(&mut T, Item) -> Ready<Option<Item>>>
	where
		F: Fn(&mut T, &Item);

	fn ready_skip_while<'a, F>(
		self,
		f: F,
	) -> SkipWhile<Self, Ready<bool>, impl FnMut(&Item) -> Ready<bool> + 'a>
	where
		F: Fn(&Item) -> bool + 'a;
}

impl<Item, S> ReadyExt<Item> for S
where
	S: Stream<Item = Item> + Sized,
{
	#[inline]
	fn ready_all<F>(self, f: F) -> All<Self, Ready<bool>, impl FnMut(Item) -> Ready<bool>>
	where
		F: Fn(Item) -> bool,
	{
		self.all(move |t| ready(f(t)))
	}

	#[inline]
	fn ready_any<F>(self, f: F) -> Any<Self, Ready<bool>, impl FnMut(Item) -> Ready<bool>>
	where
		F: Fn(Item) -> bool,
	{
		self.any(move |t| ready(f(t)))
	}

	#[inline]
	fn ready_find<'a, F>(self, f: F) -> impl Future<Output = Option<Item>> + Send
	where
		Self: Send + Unpin + 'a,
		F: Fn(&Item) -> bool + Send + 'a,
		Item: Send,
	{
		self.ready_filter(f)
			.take(1)
			.into_future()
			.map(|(curr, _next)| curr)
	}

	#[inline]
	fn ready_find_map<'a, F, U>(self, f: F) -> impl Future<Output = Option<U>> + Send
	where
		Self: Send + Unpin + 'a,
		F: Fn(Item) -> Option<U> + Send + 'a,
		Item: Send,
		U: Send,
	{
		self.ready_filter_map(f)
			.take(1)
			.into_future()
			.map(|(curr, _next)| curr)
	}

	#[inline]
	fn ready_filter<'a, F>(
		self,
		f: F,
	) -> Filter<Self, Ready<bool>, impl FnMut(&Item) -> Ready<bool> + 'a>
	where
		F: Fn(&Item) -> bool + 'a,
	{
		self.filter(move |t| ready(f(t)))
	}

	#[inline]
	fn ready_filter_map<F, U>(
		self,
		f: F,
	) -> FilterMap<Self, Ready<Option<U>>, impl FnMut(Item) -> Ready<Option<U>>>
	where
		F: Fn(Item) -> Option<U>,
	{
		self.filter_map(move |t| ready(f(t)))
	}

	#[inline]
	fn ready_fold<T, F>(
		self,
		init: T,
		f: F,
	) -> Fold<Self, Ready<T>, T, impl FnMut(T, Item) -> Ready<T>>
	where
		F: Fn(T, Item) -> T,
	{
		self.fold(init, move |a, t| ready(f(a, t)))
	}

	#[inline]
	fn ready_fold_default<T, F>(
		self,
		f: F,
	) -> Fold<Self, Ready<T>, T, impl FnMut(T, Item) -> Ready<T>>
	where
		F: Fn(T, Item) -> T,
		T: Default,
	{
		self.ready_fold(T::default(), f)
	}

	#[inline]
	#[expect(clippy::unit_arg)]
	fn ready_for_each<F>(
		self,
		mut f: F,
	) -> ForEach<Self, Ready<()>, impl FnMut(Item) -> Ready<()>>
	where
		F: FnMut(Item),
	{
		self.for_each(move |t| ready(f(t)))
	}

	#[inline]
	fn ready_take_while<'a, F>(
		self,
		f: F,
	) -> TakeWhile<Self, Ready<bool>, impl FnMut(&Item) -> Ready<bool> + 'a>
	where
		F: Fn(&Item) -> bool + 'a,
	{
		self.take_while(move |t| ready(f(t)))
	}

	#[inline]
	fn ready_scan<B, T, F>(
		self,
		init: T,
		f: F,
	) -> Scan<Self, T, Ready<Option<B>>, impl FnMut(&mut T, Item) -> Ready<Option<B>>>
	where
		F: Fn(&mut T, Item) -> Option<B>,
	{
		self.scan(init, move |s, t| ready(f(s, t)))
	}

	#[inline]
	fn ready_scan_each<T, F>(
		self,
		init: T,
		f: F,
	) -> Scan<Self, T, Ready<Option<Item>>, impl FnMut(&mut T, Item) -> Ready<Option<Item>>>
	where
		F: Fn(&mut T, &Item),
	{
		self.ready_scan(init, move |s, t| {
			f(s, &t);
			Some(t)
		})
	}

	#[inline]
	fn ready_skip_while<'a, F>(
		self,
		f: F,
	) -> SkipWhile<Self, Ready<bool>, impl FnMut(&Item) -> Ready<bool> + 'a>
	where
		F: Fn(&Item) -> bool + 'a,
	{
		self.skip_while(move |t| ready(f(t)))
	}
}
