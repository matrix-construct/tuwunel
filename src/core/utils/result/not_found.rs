use super::Result;
use crate::Error;

/// Classifies and adapts results carrying the crate's not-found error family.
///
/// Successful results and unrelated errors are not classified as not found, and
/// only a not-found error converts to an absent value. Classification
/// delegates to `Error::is_not_found`, or to the narrower `Error::is_missing`
/// for the forms that must not read a storage fault as absence.
pub trait NotFound<T> {
	/// Reports whether the result contains a not-found error.
	///
	/// An `Ok` value always returns false. Other error variants also return
	/// false without changing the result.
	#[must_use]
	fn is_not_found(&self) -> bool;

	/// Converts a not-found error into an absent value.
	///
	/// An `Ok` value is wrapped in `Some` and a not-found error becomes
	/// `Ok(None)`, so a caller distinguishes an absent value from a failed
	/// operation. Every other error is returned unchanged.
	fn optional(self) -> Result<Option<T>>;

	/// Reports whether the result contains a missing-record error.
	///
	/// Unlike `is_not_found`, an I/O not-found or any other error mapping to
	/// 404 returns false.
	#[must_use]
	fn is_missing(&self) -> bool;

	/// Converts only a missing-record error into an absent value.
	///
	/// Like `optional`, but an I/O not-found and any other error mapping to
	/// 404 are returned unchanged, so a storage fault never reads as absence.
	fn present(self) -> Result<Option<T>>;
}

impl<T> NotFound<T> for Result<T, Error> {
	#[inline]
	fn is_not_found(&self) -> bool { self.as_ref().is_err_and(Error::is_not_found) }

	#[inline]
	fn optional(self) -> Result<Option<T>> { none_if(self, Error::is_not_found) }

	#[inline]
	fn is_missing(&self) -> bool { self.as_ref().is_err_and(Error::is_missing) }

	#[inline]
	fn present(self) -> Result<Option<T>> { none_if(self, Error::is_missing) }
}

#[inline]
fn none_if<T>(result: Result<T>, absent: fn(&Error) -> bool) -> Result<Option<T>> {
	result
		.map(Some)
		.or_else(|error| absent(&error).then_some(None).ok_or(error))
}
