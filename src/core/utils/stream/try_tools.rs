//! TryStreamTools for futures::TryStream
#![expect(clippy::type_complexity)]

use futures::{TryStream, TryStreamExt, future, future::Ready, stream::TryTakeWhile};

use crate::Result;

/// Adds general-purpose operations to fallible streams.
///
/// The adapters preserve the source error type and successful item order. They
/// operate lazily without collecting the stream.
pub trait TryTools<T, E, S>
where
	S: TryStream<Ok = T, Error = E, Item = Result<T, E>> + ?Sized,
	Self: TryStream + Sized,
{
	/// Limits the stream to at most `n` successful items.
	///
	/// After yielding `n` successes, the adapter consumes one additional
	/// success to detect the limit. Earlier source errors are still forwarded;
	/// with zero, they precede consumption of the first unyielded success.
	fn try_take(
		self,
		n: usize,
	) -> TryTakeWhile<
		Self,
		Ready<Result<bool, S::Error>>,
		impl FnMut(&S::Ok) -> Ready<Result<bool, S::Error>>,
	>;
}

impl<T, E, S> TryTools<T, E, S> for S
where
	S: TryStream<Ok = T, Error = E, Item = Result<T, E>> + ?Sized,
	Self: TryStream + Sized,
{
	#[inline]
	fn try_take(
		self,
		mut n: usize,
	) -> TryTakeWhile<
		Self,
		Ready<Result<bool, S::Error>>,
		impl FnMut(&S::Ok) -> Ready<Result<bool, S::Error>>,
	> {
		self.try_take_while(move |_| {
			let res = future::ok(n > 0);
			n = n.saturating_sub(1);
			res
		})
	}
}
