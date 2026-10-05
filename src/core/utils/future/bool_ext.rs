//! Short-circuiting Boolean combinators over concurrently polled futures.
//!
//! The fixed-arity forms poll their inputs in the order given, and a pass ends
//! at the first input to decide the result, leaving those after it unpolled.
//! An unpolled input runs none of the work its future defers to `poll`, which
//! for a database read is the pool dispatch and the storage I/O. The elision
//! needs the deciding input ready on its first poll, since a pending one does
//! not end the pass. Order the arguments cheapest and likeliest to decide
//! first; the iterator forms promise no such order.

#![expect(clippy::many_single_char_names, clippy::impl_trait_in_params)]

use futures::{
	FutureExt, StreamExt,
	future::{ready, try_join, try_join3, try_join4},
	stream::FuturesUnordered,
};

use crate::utils::BoolExt as _;

#[cfg(test)]
mod tests;

/// Combines Boolean futures with concurrent short-circuit logic.
///
/// Conjunction resolves false on the first false output and true only after
/// every input resolves true. Disjunction resolves true on the first true
/// output and false only after every input resolves false.
pub trait BoolExt
where
	Self: Future<Output = bool> + Send,
{
	/// Computes the disjunction of two Boolean futures.
	///
	/// Both futures are polled concurrently. The returned future resolves true
	/// on the first true output or false after both produce false.
	fn or<B>(self, b: B) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send,
		Self: Sized;

	/// Computes the conjunction of two Boolean futures.
	///
	/// Both futures are polled concurrently. The returned future resolves false
	/// on the first false output or true after both produce true.
	fn and<B>(self, b: B) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send,
		Self: Sized;

	/// Computes the conjunction of three Boolean futures.
	///
	/// The receiver and both arguments are polled concurrently. The result is
	/// true only when all three futures produce true.
	fn and2<B, C>(self, b: B, c: C) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send,
		C: Future<Output = bool> + Send,
		Self: Sized;

	/// Computes the disjunction of three Boolean futures.
	///
	/// The receiver and both arguments are polled concurrently. The result is
	/// true when any of the three futures produces true.
	fn or2<B, C>(self, b: B, c: C) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send,
		C: Future<Output = bool> + Send,
		Self: Sized;

	/// Computes the conjunction of four Boolean futures.
	///
	/// The receiver and all three arguments are polled concurrently. The result
	/// is true only when every future produces true.
	fn and3<B, C, D>(self, b: B, c: C, d: D) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send,
		C: Future<Output = bool> + Send,
		D: Future<Output = bool> + Send,
		Self: Sized;

	/// Computes the disjunction of four Boolean futures.
	///
	/// The receiver and all three arguments are polled concurrently. The result
	/// is true when any future produces true.
	fn or3<B, C, D>(self, b: B, c: C, d: D) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send,
		C: Future<Output = bool> + Send,
		D: Future<Output = bool> + Send,
		Self: Sized;
}

impl<Fut> BoolExt for Fut
where
	Fut: Future<Output = bool> + Send,
{
	fn or<B>(self, b: B) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send,
		Self: Sized,
	{
		try_join(self.map(test_not), b.map(test_not)).map(|res| res.is_err())
	}

	fn and<B>(self, b: B) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send,
		Self: Sized,
	{
		try_join(self.map(test), b.map(test)).map(|res| res.is_ok())
	}

	fn and2<B, C>(self, b: B, c: C) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send,
		C: Future<Output = bool> + Send,
		Self: Sized,
	{
		try_join3(self.map(test), b.map(test), c.map(test)).map(|res| res.is_ok())
	}

	fn or2<B, C>(self, b: B, c: C) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send,
		C: Future<Output = bool> + Send,
		Self: Sized,
	{
		try_join3(self.map(test_not), b.map(test_not), c.map(test_not)).map(|res| res.is_err())
	}

	fn and3<B, C, D>(self, b: B, c: C, d: D) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send,
		C: Future<Output = bool> + Send,
		D: Future<Output = bool> + Send,
		Self: Sized,
	{
		try_join4(self.map(test), b.map(test), c.map(test), d.map(test)).map(|res| res.is_ok())
	}

	fn or3<B, C, D>(self, b: B, c: C, d: D) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send,
		C: Future<Output = bool> + Send,
		D: Future<Output = bool> + Send,
		Self: Sized,
	{
		try_join4(self.map(test_not), b.map(test_not), c.map(test_not), d.map(test_not))
			.map(|res| res.is_err())
	}
}

/// Computes the conjunction of an iterator of Boolean futures.
///
/// The first ready false output wins at any position in the iterator.
/// Polling all inputs concurrently costs one allocation per input.
/// An empty iterator resolves to true.
pub fn and<I, F>(args: I) -> impl Future<Output = bool> + Send
where
	I: Iterator<Item = F> + Send,
	F: Future<Output = bool> + Send,
{
	args.collect::<FuturesUnordered<_>>().all(ready)
}

/// Computes the disjunction of an iterator of Boolean futures.
///
/// The first ready true output wins at any position in the iterator.
/// Polling all inputs concurrently costs one allocation per input.
/// An empty iterator resolves to false.
pub fn or<I, F>(args: I) -> impl Future<Output = bool> + Send
where
	I: Iterator<Item = F> + Send,
	F: Future<Output = bool> + Send,
{
	args.collect::<FuturesUnordered<_>>().any(ready)
}

/// Computes the conjunction of four Boolean futures.
///
/// All four inputs are polled concurrently. The result is true only when every
/// future resolves to true.
pub fn and4(
	a: impl Future<Output = bool> + Send,
	b: impl Future<Output = bool> + Send,
	c: impl Future<Output = bool> + Send,
	d: impl Future<Output = bool> + Send,
) -> impl Future<Output = bool> + Send {
	a.and3(b, c, d)
}

/// Computes the conjunction of five Boolean futures.
///
/// All five inputs are polled concurrently. The result is true only when every
/// future resolves to true.
pub fn and5(
	a: impl Future<Output = bool> + Send,
	b: impl Future<Output = bool> + Send,
	c: impl Future<Output = bool> + Send,
	d: impl Future<Output = bool> + Send,
	e: impl Future<Output = bool> + Send,
) -> impl Future<Output = bool> + Send {
	a.and2(b, c).and2(d, e)
}

/// Computes the conjunction of six Boolean futures.
///
/// All six inputs are polled concurrently. The result is true only when every
/// future resolves to true.
pub fn and6(
	a: impl Future<Output = bool> + Send,
	b: impl Future<Output = bool> + Send,
	c: impl Future<Output = bool> + Send,
	d: impl Future<Output = bool> + Send,
	e: impl Future<Output = bool> + Send,
	f: impl Future<Output = bool> + Send,
) -> impl Future<Output = bool> + Send {
	a.and3(b, c, d).and2(e, f)
}

/// Computes the conjunction of seven Boolean futures.
///
/// All seven inputs are polled concurrently. The result is true only when every
/// future resolves to true.
pub fn and7(
	a: impl Future<Output = bool> + Send,
	b: impl Future<Output = bool> + Send,
	c: impl Future<Output = bool> + Send,
	d: impl Future<Output = bool> + Send,
	e: impl Future<Output = bool> + Send,
	f: impl Future<Output = bool> + Send,
	g: impl Future<Output = bool> + Send,
) -> impl Future<Output = bool> + Send {
	a.and3(b, c, d).and3(e, f, g)
}

fn test(test: bool) -> crate::Result<(), ()> { test.into_result() }

fn test_not(test: bool) -> crate::Result<(), ()> { test.is_false().into_result() }
