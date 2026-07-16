//! Wideband stream combinator extensions to futures::Stream

use std::convert::identity;

use futures::stream::{Stream, StreamExt};

use super::{ReadyExt, automatic_width};

/// Concurrency extensions to augment futures::StreamExt. wideband_ combinators
/// produce in-order.
pub trait WidebandExt<Item>
where
	Self: Stream<Item = Item> + Send + Sized,
{
	/// Concurrent filter_map(); ordered results
	fn widen_filter_map<F, Fut, U, N>(self, n: N, f: F) -> impl Stream<Item = U> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Item) -> Fut + Send,
		Fut: Future<Output = Option<U>> + Send,
		U: Send;

	fn widen_then<F, Fut, U, N>(self, n: N, f: F) -> impl Stream<Item = U> + Send
	where
		N: Into<Option<usize>>,
		F: Fn(Item) -> Fut + Send,
		Fut: Future<Output = U> + Send,
		U: Send;

	#[inline]
	fn wide_filter_map<F, Fut, U>(self, f: F) -> impl Stream<Item = U> + Send
	where
		F: Fn(Item) -> Fut + Send,
		Fut: Future<Output = Option<U>> + Send,
		U: Send,
	{
		self.widen_filter_map(None, f)
	}

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
