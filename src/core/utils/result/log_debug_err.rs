use std::fmt::Debug;

use tracing::Level;

use super::{DebugInspect, Result};
use crate::error;

/// Logs debug-formatted errors when debug assertions are enabled.
///
/// Successful values pass through without producing a record. With debug
/// assertions disabled, errors also pass through without producing a record.
pub trait LogDebugErr<T, E: Debug> {
	/// Logs a debug-formatted error at `level` and returns the original result.
	///
	/// The error is formatted with [`Debug`] when debug assertions are enabled.
	/// Successful values always produce no log record.
	#[must_use]
	fn err_debug_log(self, level: Level) -> Self;

	/// Logs a debug-formatted error at the error tracing level.
	///
	/// [`LogDebugErr::err_debug_log`] supplies this convenience form with
	/// [`Level::ERROR`]. Logging occurs only when debug assertions are enabled,
	/// and the original result is returned after inspection.
	#[must_use]
	fn log_debug_err(self) -> Self
	where
		Self: Sized,
	{
		self.err_debug_log(Level::ERROR)
	}
}

impl<T, E: Debug> LogDebugErr<T, E> for Result<T, E> {
	#[inline]
	fn err_debug_log(self, level: Level) -> Self {
		self.debug_inspect_err(|error| error::inspect_debug_log_level(&error, level))
	}
}
