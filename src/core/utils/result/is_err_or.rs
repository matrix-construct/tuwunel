#![expect(clippy::wrong_self_convention)]

use super::Result;

/// Tests whether a result is an error or its success value matches a predicate.
///
/// Every `Err` result returns true without exposing the error. An `Ok` result
/// is consumed and passed to the predicate.
pub trait IsErrOr<T> {
	/// Returns true for any error or a matching success value.
	///
	/// The predicate is called only for the `Ok` branch. Both the success value
	/// and any error are consumed.
	fn is_err_or<F: FnOnce(T) -> bool>(self, f: F) -> bool;
}

impl<T, E> IsErrOr<T> for Result<T, E> {
	#[inline]
	fn is_err_or<F>(self, f: F) -> bool
	where
		F: FnOnce(T) -> bool,
	{
		self.map_or(true, f)
	}
}
