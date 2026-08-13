//! Extended external extensions to futures::FutureExt
#![expect(clippy::many_single_char_names, clippy::impl_trait_in_params)]

use futures::{
	FutureExt,
	future::{
		Either::{Left, Right},
		select_ok, try_join, try_join_all, try_join3, try_join4,
	},
};

use crate::utils::BoolExt as _;

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
		B: Future<Output = bool> + Send + Unpin,
		Self: Sized + Unpin;

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
}

impl<Fut> BoolExt for Fut
where
	Fut: Future<Output = bool> + Send,
{
	fn or<B>(self, b: B) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send + Unpin,
		Self: Sized + Unpin,
	{
		select_ok([Left(self.map(test)), Right(b.map(test))]).map(|res| res.is_ok())
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

	fn and3<B, C, D>(self, b: B, c: C, d: D) -> impl Future<Output = bool> + Send
	where
		B: Future<Output = bool> + Send,
		C: Future<Output = bool> + Send,
		D: Future<Output = bool> + Send,
		Self: Sized,
	{
		try_join4(self.map(test), b.map(test), c.map(test), d.map(test)).map(|res| res.is_ok())
	}
}

/// Computes the conjunction of an iterator of Boolean futures.
///
/// All inputs are polled concurrently and false short-circuits the operation.
/// An empty iterator resolves to true.
pub fn and<I, F>(args: I) -> impl Future<Output = bool> + Send
where
	I: Iterator<Item = F> + Send,
	F: Future<Output = bool> + Send,
{
	let args = args.map(|a| a.map(test));

	try_join_all(args).map(|res| res.is_ok())
}

/// Computes the disjunction of an iterator of Boolean futures.
///
/// All inputs are polled concurrently and true short-circuits the operation.
/// False is returned only after every input resolves to false.
///
/// # Panics
///
/// Panics when the iterator contains no futures.
pub fn or<I, F>(args: I) -> impl Future<Output = bool> + Send
where
	I: Iterator<Item = F> + Send,
	F: Future<Output = bool> + Send + Unpin,
{
	let args = args.map(|a| a.map(test));

	select_ok(args).map(|res| res.is_ok())
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
