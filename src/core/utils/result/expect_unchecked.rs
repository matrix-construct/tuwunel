use std::fmt::Debug;

use super::Result;

pub trait ExpectUnchecked<T> {
	/// Returns the contained Ok value, consuming self. In debug-mode an Err
	/// panics with msg; in release-mode the check is elided and an Err is
	/// undefined behavior.
	///
	/// # Safety
	///
	/// The caller guarantees the Result is not Err.
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
