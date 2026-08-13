//! Broadband stream combinator extensions to futures::Stream

use std::convert::identity;

use futures::stream::{Stream, StreamExt};

use super::{ReadyExt, automatic_width};

/// Adds concurrent transformations with completion-ordered output.
///
/// Item futures may run ahead of downstream demand. Outputs are yielded as they
/// become ready rather than in source order.
pub trait BroadbandExt<Item>
where
	Self: Stream<Item = Item> + Send + Sized,
{
	/// Tests all items concurrently with an explicit width.
	///
	/// `n` limits in-flight predicates, while `None` selects the automatic
	/// width. An explicit zero cannot make progress. False short-circuits the
	/// operation, and an empty stream resolves true.
	fn broadn_all<F, Fut, N>(self, n: N, f: F) -> impl Future<Output = bool> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Item) -> Fut + Send,
		Fut: Future<Output = bool> + Send;

	/// Tests items concurrently for any match with an explicit width.
	///
	/// `n` limits in-flight predicates, while `None` selects the automatic
	/// width. An explicit zero cannot make progress. True short-circuits the
	/// operation, and an empty stream resolves false.
	fn broadn_any<F, Fut, N>(self, n: N, f: F) -> impl Future<Output = bool> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Item) -> Fut + Send,
		Fut: Future<Output = bool> + Send;

	/// Maps and filters items concurrently with an explicit width.
	///
	/// `n` limits in-flight item futures, while `None` selects the automatic
	/// width. An explicit zero cannot make progress. Present outputs are
	/// yielded in completion order and absent outputs are omitted.
	fn broadn_filter_map<F, Fut, U, N>(self, n: N, f: F) -> impl Stream<Item = U> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Item) -> Fut + Send,
		Fut: Future<Output = Option<U>> + Send,
		U: Send;

	/// Finds the first concurrently completed mapped value.
	///
	/// `n` limits in-flight item futures, while `None` selects the automatic
	/// width. An explicit zero cannot make progress. The first completed `Some`
	/// wins, which may differ from the earliest matching source item.
	fn broadn_find_map<'a, F, Fut, U, N>(
		self,
		n: N,
		f: F,
	) -> impl Future<Output = Option<U>> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Item) -> Fut + Send + 'a,
		Fut: Future<Output = Option<U>> + Send,
		U: Send + 'a,
		Self: Unpin + 'a;

	/// Flattens mapped streams concurrently with an explicit width.
	///
	/// A nonzero `n` limits active inner streams, zero disables the limit, and
	/// `None` selects the automatic width. Inner outputs are interleaved
	/// according to readiness rather than source order.
	fn broadn_flat_map<F, Fut, U, N>(self, n: N, f: F) -> impl Stream<Item = U> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Item) -> Fut + Send,
		Fut: Stream<Item = U> + Send + Unpin,
		U: Send;

	/// Maps items concurrently with an explicit width.
	///
	/// `n` limits in-flight item futures, while `None` selects the automatic
	/// width. An explicit zero cannot make progress. Outputs are yielded in
	/// completion order.
	fn broadn_then<F, Fut, U, N>(self, n: N, f: F) -> impl Stream<Item = U> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Item) -> Fut + Send,
		Fut: Future<Output = U> + Send,
		U: Send;

	/// Tests all items concurrently with the automatic width.
	///
	/// False short-circuits the operation, while true requires every predicate
	/// to return true. An empty stream resolves true.
	#[inline]
	fn broad_all<F, Fut>(self, f: F) -> impl Future<Output = bool> + Send
	where
		F: Fn(Item) -> Fut + Send,
		Fut: Future<Output = bool> + Send,
	{
		self.broadn_all(None, f)
	}

	/// Tests items concurrently for any match with the automatic width.
	///
	/// True short-circuits the operation, while false requires every predicate
	/// to return false. An empty stream resolves false.
	#[inline]
	fn broad_any<F, Fut>(self, f: F) -> impl Future<Output = bool> + Send
	where
		F: Fn(Item) -> Fut + Send,
		Fut: Future<Output = bool> + Send,
	{
		self.broadn_any(None, f)
	}

	/// Maps and filters items concurrently with the automatic width.
	///
	/// Present outputs are yielded in completion order rather than source
	/// order. Absent outputs are omitted.
	#[inline]
	fn broad_filter_map<F, Fut, U>(self, f: F) -> impl Stream<Item = U> + Send
	where
		F: Fn(Item) -> Fut + Send,
		Fut: Future<Output = Option<U>> + Send,
		U: Send,
	{
		self.broadn_filter_map(None, f)
	}

	/// Finds the first mapped value to complete with `Some`.
	///
	/// Item futures run at the automatic width. Completion order determines the
	/// winner rather than source order.
	#[inline]
	fn broad_find_map<'a, F, Fut, U>(self, f: F) -> impl Future<Output = Option<U>> + Send
	where
		F: Fn(Item) -> Fut + Send + 'a,
		Fut: Future<Output = Option<U>> + Send,
		U: Send + 'a,
		Self: Unpin + 'a,
	{
		self.broadn_find_map(None, f)
	}

	/// Flattens mapped streams concurrently with the automatic width.
	///
	/// Inner streams are polled together and their outputs are interleaved by
	/// readiness. Source ordering is not preserved.
	#[inline]
	fn broad_flat_map<F, Fut, U>(self, f: F) -> impl Stream<Item = U> + Send
	where
		F: Fn(Item) -> Fut + Send,
		Fut: Stream<Item = U> + Send + Unpin,
		U: Send,
	{
		self.broadn_flat_map(None, f)
	}

	/// Maps items concurrently with the automatic width.
	///
	/// Item futures may run ahead of downstream demand. Outputs are yielded in
	/// completion order rather than source order.
	#[inline]
	fn broad_then<F, Fut, U>(self, f: F) -> impl Stream<Item = U> + Send
	where
		F: Fn(Item) -> Fut + Send,
		Fut: Future<Output = U> + Send,
		U: Send,
	{
		self.broadn_then(None, f)
	}
}

impl<Item, S> BroadbandExt<Item> for S
where
	S: Stream<Item = Item> + Send + Sized,
{
	#[inline]
	fn broadn_all<F, Fut, N>(self, n: N, f: F) -> impl Future<Output = bool> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Item) -> Fut + Send,
		Fut: Future<Output = bool> + Send,
	{
		self.map(f)
			.buffer_unordered(n.into().unwrap_or_else(automatic_width))
			.ready_all(identity)
	}

	#[inline]
	fn broadn_any<F, Fut, N>(self, n: N, f: F) -> impl Future<Output = bool> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Item) -> Fut + Send,
		Fut: Future<Output = bool> + Send,
	{
		self.map(f)
			.buffer_unordered(n.into().unwrap_or_else(automatic_width))
			.ready_any(identity)
	}

	#[inline]
	fn broadn_filter_map<F, Fut, U, N>(self, n: N, f: F) -> impl Stream<Item = U> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Item) -> Fut + Send,
		Fut: Future<Output = Option<U>> + Send,
		U: Send,
	{
		self.map(f)
			.buffer_unordered(n.into().unwrap_or_else(automatic_width))
			.ready_filter_map(identity)
	}

	#[inline]
	fn broadn_find_map<'a, F, Fut, U, N>(
		self,
		n: N,
		f: F,
	) -> impl Future<Output = Option<U>> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Item) -> Fut + Send + 'a,
		Fut: Future<Output = Option<U>> + Send,
		U: Send + 'a,
		Self: Unpin + 'a,
	{
		self.map(f)
			.buffer_unordered(n.into().unwrap_or_else(automatic_width))
			.ready_find_map(identity)
	}

	#[inline]
	fn broadn_flat_map<F, Fut, U, N>(self, n: N, f: F) -> impl Stream<Item = U> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Item) -> Fut + Send,
		Fut: Stream<Item = U> + Send + Unpin,
		U: Send,
	{
		self.flat_map_unordered(n.into().unwrap_or_else(automatic_width), f)
	}

	#[inline]
	fn broadn_then<F, Fut, U, N>(self, n: N, f: F) -> impl Stream<Item = U> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Item) -> Fut + Send,
		Fut: Future<Output = U> + Send,
		U: Send,
	{
		self.map(f)
			.buffer_unordered(n.into().unwrap_or_else(automatic_width))
	}
}
