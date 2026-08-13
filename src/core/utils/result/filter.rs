use super::Result;

/// Adds fallible predicate filtering to successful results.
///
/// Existing errors bypass the predicate unchanged. Predicate failures are
/// converted into the result's error type.
pub trait Filter<T, E> {
	/// Retains an `Ok` value when `predicate` succeeds.
	///
	/// The predicate receives a shared reference to the success value. Its
	/// error is converted into `E`, while an existing `Err` bypasses the
	/// predicate.
	#[must_use]
	fn filter<P, U>(self, predicate: P) -> Self
	where
		P: FnOnce(&T) -> Result<(), U>,
		E: From<U>;
}

impl<T, E> Filter<T, E> for Result<T, E> {
	#[inline]
	fn filter<P, U>(self, predicate: P) -> Self
	where
		P: FnOnce(&T) -> Result<(), U>,
		E: From<U>,
	{
		self.and_then(move |t| predicate(&t).map(move |()| t).map_err(Into::into))
	}
}
