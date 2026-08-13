use std::fmt::Debug;

use super::Result;

/// Applies an expectation to a nested optional or fallible value.
///
/// Implementations cover `Option<Result<T, E>>` and `Result<Option<T>, E>`.
/// Only the nested value is unwrapped, preserving the outer container.
pub trait MapExpect<'a, T> {
	/// Unwraps the nested value with the supplied expectation message.
	///
	/// The outer `Option` or `Result` is preserved. The message is used only
	/// when the nested value is absent or failed.
	///
	/// # Panics
	///
	/// Panics when the nested value does not contain a success value.
	fn map_expect(self, msg: &'a str) -> T;
}

impl<'a, T, E: Debug> MapExpect<'a, Option<T>> for Option<Result<T, E>> {
	#[inline]
	fn map_expect(self, msg: &'a str) -> Option<T> { self.map(|result| result.expect(msg)) }
}

impl<'a, T, E: Debug> MapExpect<'a, Result<T, E>> for Result<Option<T>, E> {
	#[inline]
	fn map_expect(self, msg: &'a str) -> Result<T, E> { self.map(|result| result.expect(msg)) }
}
