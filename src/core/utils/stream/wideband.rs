//! Wideband stream combinator extensions to futures::Stream

use std::convert::identity;

use futures::stream::{Stream, StreamExt};

use super::{ReadyExt, automatic_width};

/// Adds bounded concurrent transformations that preserve stream order.
///
/// Multiple item futures may run ahead of downstream demand. Completed outputs
/// are held until every earlier input has produced its output.
pub trait WidebandExt<Item>
where
	Self: Stream<Item = Item> + Send + Sized,
{
	/// Maps and filters items concurrently with an explicit width.
	///
	/// `n` limits in-flight item futures, while `None` selects the automatic
	/// width; an explicit zero cannot make progress. Present outputs retain
	/// source order and absent outputs are omitted.
	fn widen_filter_map<F, Fut, U, N>(self, n: N, f: F) -> impl Stream<Item = U> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Item) -> Fut + Send,
		Fut: Future<Output = Option<U>> + Send,
		U: Send;

	/// Maps items concurrently with an explicit width while preserving order.
	///
	/// `n` limits in-flight item futures, while `None` selects the automatic
	/// width; an explicit zero cannot make progress. Item futures may run
	/// ahead, but outputs retain source order.
	fn widen_then<F, Fut, U, N>(self, n: N, f: F) -> impl Stream<Item = U> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Item) -> Fut + Send,
		Fut: Future<Output = U> + Send,
		U: Send;

	/// Maps and filters items concurrently with the automatic width.
	///
	/// Present outputs retain source order even when their futures complete out
	/// of order. Absent outputs are omitted.
	#[inline]
	fn wide_filter_map<F, Fut, U>(self, f: F) -> impl Stream<Item = U> + Send
	where
		F: Fn(Item) -> Fut + Send,
		Fut: Future<Output = Option<U>> + Send,
		U: Send,
	{
		self.widen_filter_map(None, f)
	}

	/// Maps items concurrently with the automatic width while preserving order.
	///
	/// Item futures may run ahead of downstream demand. Completed outputs wait
	/// for every earlier input before being yielded.
	#[inline]
	fn wide_then<F, Fut, U>(self, f: F) -> impl Stream<Item = U> + Send
	where
		F: Fn(Item) -> Fut + Send,
		Fut: Future<Output = U> + Send,
		U: Send,
	{
		self.widen_then(None, f)
	}
}

impl<Item, S> WidebandExt<Item> for S
where
	S: Stream<Item = Item> + Send + Sized,
{
	#[inline]
	fn widen_filter_map<F, Fut, U, N>(self, n: N, f: F) -> impl Stream<Item = U> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Item) -> Fut + Send,
		Fut: Future<Output = Option<U>> + Send,
		U: Send,
	{
		self.map(f)
			.buffered(n.into().unwrap_or_else(automatic_width))
			.ready_filter_map(identity)
	}

	#[inline]
	fn widen_then<F, Fut, U, N>(self, n: N, f: F) -> impl Stream<Item = U> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Item) -> Fut + Send,
		Fut: Future<Output = U> + Send,
		U: Send,
	{
		self.map(f)
			.buffered(n.into().unwrap_or_else(automatic_width))
	}
}
