use std::convert::identity;

use super::Result;

/// Returns the Ok value or the Err value. Available when the Ok and Err types
/// are the same. This is a way to default the result using the specific Err
/// value rather than unwrap_or_default() using Ok's default.
pub trait UnwrapOrErr<T> {
	fn unwrap_or_err(self) -> T;
}

impl<T> UnwrapOrErr<T> for Result<T, T> {
	#[inline]
	fn unwrap_or_err(self) -> T {
		self.unwrap_or_else(identity::<T>)
	}
}
