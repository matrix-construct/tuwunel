use std::{convert::Infallible, error::Error as StdError, fmt, iter::successors};

use tracing::Level;

use super::Error;

/// Flatten an error's `source()` chain into one `; caused by: ` string.
///
/// Storage and HTTP backends wrap transport errors several layers deep;
/// the outer Display can show "error sending request" while the actual
/// cause (e.g. rustls `UnknownIssuer`, hyper connect failure) is only
/// reachable via `source()`. Logging the full chain at the failure site
/// makes these self-diagnosing without per-crate trace logging.
#[must_use]
pub fn error_chain(e: &dyn StdError) -> String {
	successors(Some(e), |&e| e.source())
		.map(ToString::to_string)
		.collect::<Vec<_>>()
		.join("; caused by: ")
}

/// Logs an error and recovers with a default value in an infallible result.
///
/// The input is converted to [`Error`] and emitted through display formatting.
/// The returned `Ok` contains [`Default::default`] for the requested value
/// type.
#[inline]
pub fn else_log<T, E>(error: E) -> Result<T, Infallible>
where
	T: Default,
	Error: From<E>,
{
	Ok(default_log(error))
}

/// Debug-logs an error and recovers with a default value in an infallible
/// result.
///
/// The input is converted to [`Error`] and emitted through the debug-aware
/// logging path. The returned `Ok` contains [`Default::default`] for the
/// requested value type.
#[inline]
pub fn else_debug_log<T, E>(error: E) -> Result<T, Infallible>
where
	T: Default,
	Error: From<E>,
{
	Ok(default_debug_log(error))
}

/// Logs an error and returns a default value.
///
/// The input is converted to [`Error`] before display-formatted logging. The
/// result is independent of the error and comes from [`Default::default`].
#[inline]
pub fn default_log<T, E>(error: E) -> T
where
	T: Default,
	Error: From<E>,
{
	let error = Error::from(error);
	inspect_log(&error);
	T::default()
}

/// Debug-logs an error and returns a default value.
///
/// The input is converted to [`Error`] before using the debug-aware logging
/// path. The result is independent of the error and comes from
/// [`Default::default`].
#[inline]
pub fn default_debug_log<T, E>(error: E) -> T
where
	T: Default,
	Error: From<E>,
{
	let error = Error::from(error);
	inspect_debug_log(&error);
	T::default()
}

/// Converts and logs an error while returning the converted value.
///
/// Display-formatted logging occurs after conversion to [`Error`]. The same
/// converted error is returned for propagation by combinator chains.
#[inline]
pub fn map_log<E>(error: E) -> Error
where
	Error: From<E>,
{
	let error = Error::from(error);
	inspect_log(&error);
	error
}

/// Converts and debug-logs an error while returning the converted value.
///
/// Debug-aware logging occurs after conversion to [`Error`]. The same converted
/// error is returned for propagation by combinator chains.
#[inline]
pub fn map_debug_log<E>(error: E) -> Error
where
	Error: From<E>,
{
	let error = Error::from(error);
	inspect_debug_log(&error);
	error
}

/// Logs an error's display representation at error level.
///
/// The value is borrowed and otherwise left unchanged. Level dispatch is
/// delegated to [`inspect_log_level`].
#[inline]
pub fn inspect_log<E: fmt::Display>(error: &E) { inspect_log_level(error, Level::ERROR); }

/// Logs an error's debug representation through the debug-aware error path.
///
/// The value is borrowed and otherwise left unchanged. Level dispatch is
/// delegated to [`inspect_debug_log_level`].
#[inline]
pub fn inspect_debug_log<E: fmt::Debug>(error: &E) {
	inspect_debug_log_level(error, Level::ERROR);
}

/// Logs a display-formatted error at the selected tracing level.
///
/// Each tracing level maps to its corresponding project logging macro. The
/// value is borrowed and otherwise left unchanged.
#[inline]
pub fn inspect_log_level<E: fmt::Display>(error: &E, level: Level) {
	use crate::{debug, error, info, trace, warn};

	match level {
		| Level::ERROR => error!("{error}"),
		| Level::WARN => warn!("{error}"),
		| Level::INFO => info!("{error}"),
		| Level::DEBUG => debug!("{error}"),
		| Level::TRACE => trace!("{error}"),
	}
}

/// Logs a debug-formatted error at the selected debug-aware tracing level.
///
/// Error, warning, and information inputs use their debug-sensitive logging
/// macros, while debug and trace use fixed levels. The value is borrowed and
/// otherwise left unchanged.
#[inline]
pub fn inspect_debug_log_level<E: fmt::Debug>(error: &E, level: Level) {
	use crate::{debug, debug_error, debug_info, debug_warn, trace};

	match level {
		| Level::ERROR => debug_error!("{error:?}"),
		| Level::WARN => debug_warn!("{error:?}"),
		| Level::INFO => debug_info!("{error:?}"),
		| Level::DEBUG => debug!("{error:?}"),
		| Level::TRACE => trace!("{error:?}"),
	}
}
