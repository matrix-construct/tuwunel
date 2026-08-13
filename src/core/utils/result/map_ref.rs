use super::Result;

/// Maps a successful result through a shared reference to its value.
///
/// The operation borrows the owned success value for the duration of the
/// callback. An existing error is forwarded unchanged.
pub trait MapRef<T, E> {
	/// Applies `op` to a shared reference inside an `Ok` result.
	///
	/// The original success value is dropped after the callback returns. An
	/// `Err` result bypasses the callback and preserves its error.
	fn map_ref<U, F>(self, op: F) -> Result<U, E>
	where
		F: FnOnce(&T) -> U;
}

impl<T, E> MapRef<T, E> for Result<T, E> {
	#[inline]
	fn map_ref<U, F>(self, op: F) -> Result<U, E>
	where
		F: FnOnce(&T) -> U,
	{
		self.map(|t| op(&t))
	}
}
