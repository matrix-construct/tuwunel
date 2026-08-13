use std::convert::Infallible;

use super::{DebugInspect, Result};
use crate::error;

/// Extracts the value from a result whose error type is uninhabited.
///
/// Only the `Ok` branch can be constructed for `Result<T, Infallible>`. The
/// implementation uses an unchecked unwrap after a debug assertion.
pub trait UnwrapInfallible<T> {
	/// Returns the only constructible value from the result.
	///
	/// The uninhabited error type makes the `Err` branch unreachable. Consuming
	/// the result requires no fallback value or closure.
	fn unwrap_infallible(self) -> T;
}

impl<T> UnwrapInfallible<T> for Result<T, Infallible> {
	#[inline]
	fn unwrap_infallible(self) -> T {
		// SAFETY: Branchless unwrap for errors that can never happen. In debug
		// mode this is asserted.
		unsafe {
			self.debug_inspect_err(error::infallible)
				.unwrap_unchecked()
		}
	}
}
