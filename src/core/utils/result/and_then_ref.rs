use super::Result;

/// Chains a fallible operation over a shared reference to a success value.
///
/// The owned value is borrowed only while the callback runs. An existing error
/// is forwarded without invoking the callback.
pub trait AndThenRef<T, E> {
	/// Applies `op` to a shared reference inside an `Ok` result.
	///
	/// The callback may replace the success type or return the same error type.
	/// The original success value is dropped after the callback completes.
	fn and_then_ref<U, F>(self, op: F) -> Result<U, E>
	where
		F: FnOnce(&T) -> Result<U, E>;
}

impl<T, E> AndThenRef<T, E> for Result<T, E> {
	#[inline]
	fn and_then_ref<U, F>(self, op: F) -> Result<U, E>
	where
		F: FnOnce(&T) -> Result<U, E>,
	{
		self.and_then(|t| op(&t))
	}
}
