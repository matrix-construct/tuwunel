use super::Result;
use crate::Error;

/// Classifies results that contain the crate's not-found error family.
///
/// Successful results and unrelated errors are not classified as missing.
/// Classification delegates to `Error::is_not_found`.
pub trait NotFound<T> {
	/// Reports whether the result contains a not-found error.
	///
	/// An `Ok` value always returns false. Other error variants also return
	/// false without changing the result.
	#[must_use]
	fn is_not_found(&self) -> bool;
}

impl<T> NotFound<T> for Result<T, Error> {
	#[inline]
	fn is_not_found(&self) -> bool { self.as_ref().is_err_and(Error::is_not_found) }
}
