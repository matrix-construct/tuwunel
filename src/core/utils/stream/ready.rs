//! Synchronous combinator extensions to futures::Stream
#![expect(clippy::type_complexity)]

use futures::{
	future::{FutureExt, Ready, ready},
	stream::{
		All, Any, Filter, FilterMap, Fold, ForEach, Scan, SkipWhile, Stream, StreamExt, TakeWhile,
	},
};

/// Adds synchronous counterparts to selected [`StreamExt`] combinators.
///
/// These methods accept ordinary closures for operations that do not need to
/// await, avoiding an async block around iterator-like predicates and folds.
pub trait ReadyExt<Item>
where
	Self: Stream<Item = Item> + Sized,
{
	/// Tests whether every item satisfies a synchronous predicate.
	///
	/// Evaluation stops at the first false result. An empty stream resolves to
	/// true.
	fn ready_all<F>(self, f: F) -> All<Self, Ready<bool>, impl FnMut(Item) -> Ready<bool>>
	where
		F: Fn(Item) -> bool;

	/// Tests whether any item satisfies a synchronous predicate.
	///
	/// Evaluation stops at the first true result. An empty stream resolves to
	/// false.
	fn ready_any<F>(self, f: F) -> Any<Self, Ready<bool>, impl FnMut(Item) -> Ready<bool>>
	where
		F: Fn(Item) -> bool;

	/// Finds the first item satisfying a synchronous predicate.
	///
	/// Items are tested in source order and the first match is returned. The
	/// future resolves to `None` when the stream ends without a match.
	fn ready_find<'a, F>(self, f: F) -> impl Future<Output = Option<Item>> + Send
	where
		Self: Send + Unpin + 'a,
		F: Fn(&Item) -> bool + Send + 'a,
		Item: Send;

	/// Finds the first value produced by a synchronous mapping predicate.
	///
	/// Items are mapped in source order until `f` returns `Some`. The future
	/// resolves to `None` when every item maps to absence.
	fn ready_find_map<'a, F, U>(self, f: F) -> impl Future<Output = Option<U>> + Send
	where
		Self: Send + Unpin + 'a,
		F: Fn(Item) -> Option<U> + Send + 'a,
		Item: Send,
		U: Send;

	/// Retains items accepted by a synchronous predicate.
	///
	/// The predicate borrows each item as it is polled. Accepted items preserve
	/// their source order and value.
	fn ready_filter<'a, F>(
		self,
		f: F,
	) -> Filter<Self, Ready<bool>, impl FnMut(&Item) -> Ready<bool> + 'a>
	where
		F: Fn(&Item) -> bool + 'a;

	/// Maps items synchronously while filtering absent results.
	///
	/// Values returned in `Some` are yielded in source order. Items mapped to
	/// `None` are discarded.
	fn ready_filter_map<F, U>(
		self,
		f: F,
	) -> FilterMap<Self, Ready<Option<U>>, impl FnMut(Item) -> Ready<Option<U>>>
	where
		F: Fn(Item) -> Option<U>;

	/// Folds items synchronously from an explicit initial accumulator.
	///
	/// `f` receives the accumulator and each item in source order. The future
	/// resolves to the final accumulator after the stream ends.
	fn ready_fold<T, F>(
		self,
		init: T,
		f: F,
	) -> Fold<Self, Ready<T>, T, impl FnMut(T, Item) -> Ready<T>>
	where
		F: Fn(T, Item) -> T;

	/// Folds items synchronously from `T::default()`.
	///
	/// `f` receives the accumulator and each item in source order. An empty
	/// stream resolves to the default accumulator unchanged.
	fn ready_fold_default<T, F>(
		self,
		f: F,
	) -> Fold<Self, Ready<T>, T, impl FnMut(T, Item) -> Ready<T>>
	where
		F: Fn(T, Item) -> T,
		T: Default;

	/// Applies a synchronous closure to every stream item.
	///
	/// Items are consumed in source order. The returned future resolves after
	/// the stream ends and carries no output value.
	fn ready_for_each<F>(self, f: F) -> ForEach<Self, Ready<()>, impl FnMut(Item) -> Ready<()>>
	where
		F: FnMut(Item);

	/// Yields the leading items accepted by a synchronous predicate.
	///
	/// Evaluation stops at the first false result, and that boundary item is
	/// not yielded. Accepted items retain source order.
	fn ready_take_while<'a, F>(
		self,
		f: F,
	) -> TakeWhile<Self, Ready<bool>, impl FnMut(&Item) -> Ready<bool> + 'a>
	where
		F: Fn(&Item) -> bool + 'a;

	/// Transforms items with mutable state and a synchronous scan closure.
	///
	/// Each `Some` result is yielded while `None` terminates the stream. State
	/// is initialized once and retained between items.
	fn ready_scan<B, T, F>(
		self,
		init: T,
		f: F,
	) -> Scan<Self, T, Ready<Option<B>>, impl FnMut(&mut T, Item) -> Ready<Option<B>>>
	where
		F: Fn(&mut T, Item) -> Option<B>;

	/// Observes each item with mutable state and yields it unchanged.
	///
	/// The closure receives the persistent state and a shared reference to each
	/// item. Every source item is then emitted in order.
	fn ready_scan_each<T, F>(
		self,
		init: T,
		f: F,
	) -> Scan<Self, T, Ready<Option<Item>>, impl FnMut(&mut T, Item) -> Ready<Option<Item>>>
	where
		F: Fn(&mut T, &Item);

	/// Skips the leading items accepted by a synchronous predicate.
	///
	/// The first false item and every later item are yielded in source order.
	/// The predicate is no longer called after the first false result.
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
