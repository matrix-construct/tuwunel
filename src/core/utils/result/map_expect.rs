use std::fmt::Debug;

use super::Result;

pub trait MapExpect<'a, T> {
	/// Calls expect(msg) on the mapped Result value. This is similar to
	/// map(Result::unwrap) but composes an expect call and message without
	/// requiring a closure.
	fn map_expect(self, msg: &'a str) -> T;
}

impl<'a, T, E: Debug> MapExpect<'a, Option<T>> for Option<Result<T, E>> {
	#[inline]
	fn map_expect(self, msg: &'a str) -> Option<T> {
		self.map(|result| result.expect(msg))
	}
}

impl<'a, T, E: Debug> MapExpect<'a, Result<T, E>> for Result<Option<T>, E> {
	#[inline]
	fn map_expect(self, msg: &'a str) -> Result<T, E> {
		self.map(|result| result.expect(msg))
	}
}
