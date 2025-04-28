use std::{convert::Infallible, fmt};

use tracing::Level;

use super::Error;

#[inline]
pub fn else_log<T, E>(error: E) -> Result<T, Infallible>
where
	T: Default,
	Error: From<E>,
{
	Ok(default_log(error))
}

#[inline]
pub fn else_debug_log<T, E>(error: E) -> Result<T, Infallible>
where
	T: Default,
	Error: From<E>,
{
	Ok(default_debug_log(error))
}

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

#[inline]
pub fn map_log<E>(error: E) -> Error
where
	Error: From<E>,
{
	let error = Error::from(error);
	inspect_log(&error);
	error
}

#[inline]
pub fn map_debug_log<E>(error: E) -> Error
where
	Error: From<E>,
{
	let error = Error::from(error);
	inspect_debug_log(&error);
	error
}

#[inline]
pub fn inspect_log<E: fmt::Display>(error: &E) {
	inspect_log_level(error, Level::ERROR);
}

#[inline]
pub fn inspect_debug_log<E: fmt::Debug>(error: &E) {
	inspect_debug_log_level(error, Level::ERROR);
}

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
