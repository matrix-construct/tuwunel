//! Error construction macros
//!
//! These are specialized macros specific to this project's patterns for
//! throwing Errors; they make Error construction succinct and reduce clutter.
//! They are developed from folding existing patterns into the macro while
//! fixing several anti-patterns in the codebase.
//!
//! - The primary macros `Err!` and `err!` are provided. `Err!` simply wraps
//!   `err!` in the Result variant to reduce `Err(err!(...))` boilerplate, thus
//!   `err!` can be used in any case.
//!
//! 1. The macro makes the general Error construction easy: `return
//!    Err!("something went wrong")` replaces the prior `return
//!    Err(Error::Err("something went wrong".to_owned()))`.
//!
//! 2. The macro integrates format strings automatically: `return
//!    Err!("something bad: {msg}")` replaces the prior `return
//!    Err(Error::Err(format!("something bad: {msg}")))`.
//!
//! 3. The macro scopes variants of Error: `return Err!(Database("problem with
//!    bad database."))` replaces the prior `return Err(Error::Database("problem
//!    with bad database."))`.
//!
//! 4. The macro matches and scopes some special-case sub-variants, for example
//!    with ruma ErrorKind: `return Err!(Request(MissingToken("you must provide
//!    an access token")))`.
//!
//! 5. The macro fixes the anti-pattern of repeating messages in an error! log
//!    and then again in an Error construction, often slightly different due to
//!    the Error variant not supporting a format string. Instead `return
//!    Err(Database(error!("problem with db: {msg}")))` logs the error at the
//!    callsite and then returns the error with the same string. Caller has the
//!    option of replacing `error!` with `debug_error!`.

#[macro_export]
#[collapse_debuginfo(yes)]
macro_rules! Err {
	($($args:tt)*) => {
		Err($crate::err!($($args)*))
	};
}

#[macro_export]
#[collapse_debuginfo(yes)]
macro_rules! err {
	(HttpJson($statuscode:ident, $($args:tt)+)) => {
		$crate::error::Error::HttpJson(
			$crate::http::StatusCode::$statuscode,
			::axum::Json(::serde_json::json!($($args)+))
		)
	};

	(Request(Forbidden($level:ident!($($args:tt)+)))) => {{
		let mut buf = String::new();
		$crate::error::Error::Request(
			$crate::ruma::api::client::error::ErrorKind::forbidden(),
			$crate::err_log!(buf, $level, $($args)+),
			$crate::http::StatusCode::BAD_REQUEST
		)
	}};

	(Request(Forbidden($($args:tt)+))) => {
		$crate::error::Error::Request(
			$crate::ruma::api::client::error::ErrorKind::forbidden(),
			$crate::format_maybe!($($args)+),
			$crate::http::StatusCode::BAD_REQUEST
		)
	};

	(Request($variant:ident($level:ident!($($args:tt)+)))) => {{
		let mut buf = String::new();
		$crate::error::Error::Request(
			$crate::ruma::api::client::error::ErrorKind::$variant,
			$crate::err_log!(buf, $level, $($args)+),
			$crate::http::StatusCode::BAD_REQUEST
		)
	}};

	(Request($variant:ident($($args:tt)+))) => {
		$crate::error::Error::Request(
			$crate::ruma::api::client::error::ErrorKind::$variant,
			$crate::format_maybe!($($args)+),
			$crate::http::StatusCode::BAD_REQUEST
		)
	};

	(Config($item:literal, $($args:tt)+)) => {{
		let mut buf = String::new();
		$crate::error::Error::Config($item, $crate::err_log!(buf, error, config = %$item, $($args)+))
	}};

	($variant:ident($level:ident!($($args:tt)+))) => {{
		let mut buf = String::new();
		$crate::error::Error::$variant($crate::err_log!(buf, $level, $($args)+))
	}};

	($variant:ident($($args:ident),+)) => {
		$crate::error::Error::$variant($($args),+)
	};

	($variant:ident($($args:tt)+)) => {
		$crate::error::Error::$variant($crate::format_maybe!($($args)+))
	};

	($level:ident!($($args:tt)+)) => {{
		let mut buf = String::new();
		$crate::error::Error::Err($crate::err_log!(buf, $level, $($args)+))
	}};

	($($args:tt)+) => {
		$crate::error::Error::Err($crate::format_maybe!($($args)+))
	};
}

/// A trinity of integration between tracing, logging, and Error. This is a
/// customization of tracing::event! with the primary purpose of sharing the
/// error string, fieldset parsing and formatting. An added benefit is that we
/// can share the same callsite metadata for the source of our Error and the
/// associated logging and tracing event dispatches.
#[macro_export]
#[collapse_debuginfo(yes)]
macro_rules! err_log {
	($out:ident, $level:ident, $($fields:tt)+) => {{
		use $crate::tracing::{
			callsite, callsite2, metadata, valueset, Callsite,
			Level,
		};

		const LEVEL: Level = $crate::err_lev!($level);
		static __CALLSITE: callsite::DefaultCallsite = callsite2! {
			name: std::concat! {
				"event ",
				std::file!(),
				":",
				std::line!(),
			},
			kind: metadata::Kind::EVENT,
			target: std::module_path!(),
			level: LEVEL,
			fields: $($fields)+,
		};

		($crate::error::visit)(
			&mut $out,
			LEVEL,
			&__CALLSITE,
			&mut valueset!(__CALLSITE.metadata().fields(), $($fields)+)
		);

		($out).into()
	}}
}

#[macro_export]
#[collapse_debuginfo(yes)]
macro_rules! err_lev {
	(debug_warn) => {
		if $crate::debug::logging() {
			$crate::tracing::Level::WARN
		} else {
			$crate::tracing::Level::DEBUG
		}
	};

	(debug_error) => {
		if $crate::debug::logging() {
			$crate::tracing::Level::ERROR
		} else {
			$crate::tracing::Level::DEBUG
		}
	};

	(warn) => {
		$crate::tracing::Level::WARN
	};

	(error) => {
		$crate::tracing::Level::ERROR
	};
}

use std::{fmt, fmt::Write};

use tracing::{
	__macro_support, __tracing_log, Callsite, Event, Level,
	callsite::DefaultCallsite,
	field::{Field, ValueSet, Visit},
	level_enabled,
};

struct Visitor<'a>(&'a mut String);

impl Visit for Visitor<'_> {
	#[inline]
	fn record_debug(&mut self, field: &Field, val: &dyn fmt::Debug) {
		if field.name() == "message" {
			write!(self.0, "{val:?}").expect("stream error");
		} else {
			write!(self.0, " {}={val:?}", field.name()).expect("stream error");
		}
	}
}

pub fn visit(
	out: &mut String,
	level: Level,
	__callsite: &'static DefaultCallsite,
	vs: &mut ValueSet<'_>,
) {
	let meta = __callsite.metadata();
	let enabled = level_enabled!(level) && {
		let interest = __callsite.interest();
		!interest.is_never() && __macro_support::__is_enabled(meta, interest)
	};

	if enabled {
		Event::dispatch(meta, vs);
	}

	__tracing_log!(level, __callsite, vs);
	vs.record(&mut Visitor(out));
}
