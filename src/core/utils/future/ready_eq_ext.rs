//! Future extension for Partial Equality against present value

use futures::FutureExt;

/// Compares a future's output with a borrowed value.
///
/// The comparison is attached to the future and runs when it resolves. Both
/// equality and inequality forms preserve asynchronous composition.
pub trait ReadyEqExt<T>
where
	Self: Future<Output = T> + Send + Sized,
	T: PartialEq + Send + Sync,
{
	/// Tests whether the future's output equals `t`.
	///
	/// The comparison borrows the supplied value until the returned future
	/// completes. The output value is consumed after comparison.
	fn eq(self, t: &T) -> impl Future<Output = bool> + Send;

	/// Tests whether the future's output differs from `t`.
	///
	/// The comparison borrows the supplied value until the returned future
	/// completes. The output value is consumed after comparison.
	fn ne(self, t: &T) -> impl Future<Output = bool> + Send;
}

impl<Fut, T> ReadyEqExt<T> for Fut
where
	Fut: Future<Output = T> + Send + Sized,
	T: PartialEq + Send + Sync,
{
	#[inline]
	fn eq(self, t: &T) -> impl Future<Output = bool> + Send { self.map(move |r| r.eq(t)) }

	#[inline]
	fn ne(self, t: &T) -> impl Future<Output = bool> + Send { self.map(move |r| r.ne(t)) }
}
