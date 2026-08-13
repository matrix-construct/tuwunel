use futures::{Stream, StreamExt, TryStream, future::ready};

use crate::{Error, Result, utils::stream::TryExpect};

/// Selects successful or failed items from a result stream.
///
/// Success selection asserts error-free input when debug assertions are enabled
/// and filters errors when they are disabled. Error selection always discards
/// successful items.
pub trait TryIgnore<Item>
where
	Item: Send,
	Self: Send + Sized,
{
	/// Yields successful values while conditionally ignoring errors.
	///
	/// Errors are filtered when debug assertions are disabled. When they are
	/// enabled, each item is expected to be successful so accidental error loss
	/// remains visible.
	///
	/// # Panics
	///
	/// Panics when debug assertions are enabled and the source yields an error.
	fn ignore_err(self) -> impl Stream<Item = Item> + Send;

	/// Yields errors while ignoring successful values.
	///
	/// Successful items are filtered out as the stream is polled. Errors retain
	/// their source order and value.
	fn ignore_ok(self) -> impl Stream<Item = Error> + Send;
}

impl<Item, S> TryIgnore<Item> for S
where
	S: Stream<Item = Result<Item>> + Send + TryStream + TryExpect<Item>,
	Item: Send,
	Self: Send + Sized,
{
	#[cfg(debug_assertions)]
	#[inline]
	fn ignore_err(self: S) -> impl Stream<Item = Item> + Send { self.expect_ok() }

	#[cfg(not(debug_assertions))]
	#[inline]
	fn ignore_err(self: S) -> impl Stream<Item = Item> + Send {
		self.filter_map(|res| ready(res.ok()))
	}

	#[inline]
	fn ignore_ok(self: S) -> impl Stream<Item = Error> + Send {
		self.filter_map(|res| ready(res.err()))
	}
}
