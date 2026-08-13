use std::fmt;

use tracing::Level;

use super::Result;
use crate::error;

/// Logs display-formatted errors while preserving their result.
///
/// Successful values pass through without producing a log record. Error
/// values are inspected at the requested tracing level and remain unchanged.
pub trait ErrLog<T, E>
where
	E: fmt::Display,
{
	/// Logs an error at `level` and returns the original result.
	///
	/// The error is formatted with [`fmt::Display`]. An `Ok` value produces no
	/// record and is returned unchanged.
	#[must_use]
	fn log_err(self, level: Level) -> Self;

	/// Logs an error at the error tracing level.
	///
	/// [`ErrLog::log_err`] supplies this convenience form with
	/// [`Level::ERROR`]. The original result is returned after inspection.
	#[inline]
	#[must_use]
	fn err_log(self) -> Self
	where
		Self: Sized,
	{
		self.log_err(Level::ERROR)
	}
}

/// Logs debug-formatted errors while preserving their result.
///
/// Successful values pass through without producing a log record. Error
/// values are inspected at the requested tracing level and remain unchanged.
pub trait ErrDebugLog<T, E>
where
	E: fmt::Debug,
{
	/// Logs an error at `level` and returns the original result.
	///
	/// The error is formatted with [`fmt::Debug`]. An `Ok` value produces no
	/// record and is returned unchanged.
	#[must_use]
	fn log_err_debug(self, level: Level) -> Self;

	/// Logs a debug-formatted error at the error tracing level.
	///
	/// [`ErrDebugLog::log_err_debug`] supplies this convenience form with
	/// [`Level::ERROR`]. The original result is returned after inspection.
	#[inline]
	#[must_use]
	fn err_debug_log(self) -> Self
	where
		Self: Sized,
	{
		self.log_err_debug(Level::ERROR)
	}
}

impl<T, E> ErrLog<T, E> for Result<T, E>
where
	E: fmt::Display,
{
	#[inline]
	fn log_err(self, level: Level) -> Self
	where
		Self: Sized,
	{
		self.inspect_err(|error| error::inspect_log_level(&error, level))
	}
}

impl<T, E> ErrDebugLog<T, E> for Result<T, E>
where
	E: fmt::Debug,
{
	#[inline]
	fn log_err_debug(self, level: Level) -> Self
	where
		Self: Sized,
	{
		self.inspect_err(|error| error::inspect_debug_log_level(&error, level))
	}
}
