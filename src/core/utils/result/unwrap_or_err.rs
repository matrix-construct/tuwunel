use std::convert::identity;

use super::Result;

/// Returns the contained value from either variant of a `Result<T, T>`.
///
/// Unlike `unwrap_or_default`, the `Err` branch retains its value instead of
/// constructing `T::default()`.
pub trait UnwrapOrErr<T> {
	/// Returns the value carried by either result variant.
	///
	/// Both variants contain the same type, so neither branch needs conversion.
	/// The result is consumed without constructing a fallback value.
	fn unwrap_or_err(self) -> T;
}

impl<T> UnwrapOrErr<T> for Result<T, T> {
	#[inline]
	fn unwrap_or_err(self) -> T { self.unwrap_or_else(identity::<T>) }
}
