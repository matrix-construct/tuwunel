use std::fmt::Debug;

use super::Result;

/// Extracts successful values through an unchecked result assertion.
///
/// Debug builds retain an assertion for the error branch. Release builds rely
/// entirely on the caller's safety guarantee.
pub trait ExpectUnchecked<T> {
	/// Returns the contained `Ok` value without a release-mode branch check.
	///
	/// Debug builds use `msg` when asserting that the result is successful.
	/// Release builds treat an `Err` value as unreachable.
	///
	/// # Panics
	///
	/// Panics in debug builds when the result is `Err`.
	///
	/// # Safety
	///
	/// The caller must guarantee the result is not `Err`; violating this in
	/// release builds causes undefined behavior.
	unsafe fn expect_unchecked(self, msg: &str) -> T;
}

impl<T, E> ExpectUnchecked<T> for Result<T, E>
where
	E: Debug,
{
	#[inline]
	unsafe fn expect_unchecked(self, msg: &str) -> T {
		if cfg!(debug_assertions) {
			self.expect(msg)
		} else {
			// SAFETY: The caller guarantees the Result is not Err.
			unsafe { self.unwrap_unchecked() }
		}
	}
}
