use super::Result;

/// Consumes a result and reduces it to success or failure.
///
/// Both contained values are discarded. The returned Boolean records only
/// whether the original result was `Ok`.
pub trait IntoIsOk<T, E> {
	/// Reports whether the consumed result is `Ok`.
	///
	/// The success value and error are both discarded. No conversion or logging
	/// is performed for either branch.
	fn into_is_ok(self) -> bool;
}

impl<T, E> IntoIsOk<T, E> for Result<T, E> {
	#[inline]
	fn into_is_ok(self) -> bool { self.is_ok() }
}
