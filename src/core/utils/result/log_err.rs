use std::fmt::Display;

use tracing::Level;

use super::Result;
use crate::error;

/// Logs display-formatted errors without changing their result.
///
/// Successful values pass through without producing a record. Errors remain
/// available to the caller after being logged.
pub trait LogErr<T, E: Display> {
	/// Logs an error at `level` and returns the original result.
	///
	/// The error is formatted with [`Display`]. Successful values produce no
	/// log record.
	#[must_use]
	fn err_log(self, level: Level) -> Self;

	/// Logs an error at the error tracing level.
	///
	/// [`LogErr::err_log`] supplies this convenience form with
	/// [`Level::ERROR`]. The original result is returned after inspection.
	#[must_use]
	fn log_err(self) -> Self
	where
		Self: Sized,
	{
		self.err_log(Level::ERROR)
	}
}

impl<T, E: Display> LogErr<T, E> for Result<T, E> {
	#[inline]
	fn err_log(self, level: Level) -> Self {
		self.inspect_err(|error| error::inspect_log_level(&error, level))
	}
}
