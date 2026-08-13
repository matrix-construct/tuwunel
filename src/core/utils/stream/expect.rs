use futures::{Stream, StreamExt, TryStream};

use crate::Result;

/// Converts fallible stream items into values by expecting success.
///
/// Successful items pass through in source order. Errors terminate the current
/// poll by panicking with either a default or caller-supplied message.
pub trait TryExpect<Item>
where
	Item: Send,
	Self: Send + Sized,
{
	/// Expects every stream item with the default failure message.
	///
	/// Successful values are unwrapped lazily as the stream is polled. The
	/// default message identifies a stream expectation failure.
	///
	/// # Panics
	///
	/// Panics when the source stream yields an error.
	fn expect_ok(self) -> impl Stream<Item = Item> + Send;

	/// Expects every stream item with `msg` as the failure message.
	///
	/// Successful values are unwrapped lazily as the stream is polled. The
	/// supplied message is used for every failing item.
	///
	/// # Panics
	///
	/// Panics when the source stream yields an error.
	fn map_expect(self, msg: &str) -> impl Stream<Item = Item> + Send;
}

impl<Item, S> TryExpect<Item> for S
where
	S: Stream<Item = Result<Item>> + Send + TryStream,
	Item: Send,
	Self: Send + Sized,
{
	#[inline]
	fn expect_ok(self: S) -> impl Stream<Item = Item> + Send {
		self.map_expect("stream expectation failure")
	}

	//TODO: move to impl MapExpect
	#[inline]
	fn map_expect(self, msg: &str) -> impl Stream<Item = Item> + Send {
		self.map(|res| res.expect(msg))
	}
}
