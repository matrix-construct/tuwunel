//! TryStreamTools for futures::TryStream
#![expect(clippy::type_complexity)]

use futures::{
	TryStream, TryStreamExt, future,
	future::{Ready, ready},
	stream::TryTakeWhile,
};

use crate::Result;

/// Adds general-purpose operations to fallible streams.
///
/// Operations preserve the source error type and process successful items in
/// source order. They support limiting the stream and collecting successful pairs.
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

	/// Collects successful pairs into two collections.
	///
	/// Collections are extended in source order, starting from their defaults.
	/// The first source error ends the fold without returning partial collections.
	fn try_unzip<FromA, FromB>(self) -> impl Future<Output = Result<(FromA, FromB), S::Error>>
	where
		(FromA, FromB): Default + Extend<T>;
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

	#[inline]
	fn try_unzip<FromA, FromB>(self) -> impl Future<Output = Result<(FromA, FromB), S::Error>>
	where
		(FromA, FromB): Default + Extend<T>,
	{
		self.try_fold(<(FromA, FromB)>::default(), |mut collections, item| {
			collections.extend([item]);
			ready(Ok(collections))
		})
	}
}
