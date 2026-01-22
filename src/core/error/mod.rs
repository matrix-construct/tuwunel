mod err;
mod log;
mod panic;
mod response;
mod serde;

use std::{
	any::Any,
	borrow::Cow,
	convert::Infallible,
	sync::{Mutex, PoisonError},
};

pub use self::{err::visit, log::*};
use crate::utils::{assert_ref_unwind_safe, assert_send, assert_sync, assert_unwind_safe};

#[derive(thiserror::Error)]
pub enum Error {
	#[error("PANIC!")]
	PanicAny(Mutex<Box<dyn Any + Send>>),
	#[error("PANIC! {0}")]
	Panic(&'static str, Mutex<Box<dyn Any + Send + 'static>>),

	// std
	#[error(transparent)]
	Fmt(#[from] std::fmt::Error),
	#[error(transparent)]
	FromUtf8(#[from] std::string::FromUtf8Error),
	#[error("I/O error: {0}")]
	Io(#[from] std::io::Error),
	#[error(transparent)]
	ParseFloat(#[from] std::num::ParseFloatError),
	#[error(transparent)]
	ParseInt(#[from] std::num::ParseIntError),
	#[error(transparent)]
	Std(#[from] Box<dyn std::error::Error + Send + Sync + 'static>),
	#[error(transparent)]
	SystemTime(#[from] std::time::SystemTimeError),
	#[error(transparent)]
	ThreadAccessError(#[from] std::thread::AccessError),
	#[error(transparent)]
	TryFromInt(#[from] std::num::TryFromIntError),
	#[error(transparent)]
	TryFromSlice(#[from] std::array::TryFromSliceError),
	#[error(transparent)]
	Utf8(#[from] std::str::Utf8Error),

	// third-party
	#[error(transparent)]
	CapacityError(#[from] arrayvec::CapacityError),
	#[error(transparent)]
	CargoToml(#[from] cargo_toml::Error),
	#[error(transparent)]
	Clap(#[from] clap::error::Error),
	#[error(transparent)]
	Extension(#[from] axum::extract::rejection::ExtensionRejection),
	#[error(transparent)]
	Figment(#[from] figment::error::Error),
	#[error(transparent)]
	HtmlFormDe(#[from] serde_html_form::de::Error),
	#[error(transparent)]
	HtmlFormSer(#[from] serde_html_form::ser::Error),
	#[error(transparent)]
	Http(#[from] http::Error),
	#[error(transparent)]
	HttpHeader(#[from] http::header::InvalidHeaderValue),
	#[error("Join error: {0}")]
	JoinError(#[from] tokio::task::JoinError),
	#[error(transparent)]
	Json(#[from] serde_json::Error),
	#[error(transparent)]
	JsParseInt(#[from] ruma::JsParseIntError), // js_int re-export
	#[error(transparent)]
	JsTryFromInt(#[from] ruma::JsTryFromIntError), // js_int re-export
	#[error(transparent)]
	Path(#[from] axum::extract::rejection::PathRejection),
	#[error("Mutex poisoned: {0}")]
	Poison(Cow<'static, str>),
	#[error("Regex error: {0}")]
	Regex(#[from] regex::Error),
	#[error("Request error: {0}")]
	Reqwest(#[from] reqwest::Error),
	#[error("{0}")]
	SerdeDe(Cow<'static, str>),
	#[error("{0}")]
	SerdeSer(Cow<'static, str>),
	#[error(transparent)]
	TomlDe(#[from] toml::de::Error),
	#[error(transparent)]
	TomlSer(#[from] toml::ser::Error),
	#[error("Tracing filter error: {0}")]
	TracingFilter(#[from] tracing_subscriber::filter::ParseError),
	#[error("Tracing reload error: {0}")]
	TracingReload(#[from] tracing_subscriber::reload::Error),
	#[error(transparent)]
	TypedHeader(#[from] axum_extra::typed_header::TypedHeaderRejection),
	#[error(transparent)]
	UrlParse(#[from] url::ParseError),
	#[error(transparent)]
	Yaml(#[from] serde_yaml::Error),

	// ruma/tuwunel
	#[error("Arithmetic operation failed: {0}")]
	Arithmetic(Cow<'static, str>),
	#[error("{0}: {1}")]
	BadRequest(ruma::api::client::error::ErrorKind, &'static str), //TODO: remove
	#[error("{0}")]
	BadServerResponse(Cow<'static, str>),
	#[error(transparent)]
	CanonicalJson(#[from] ruma::CanonicalJsonError),
	#[error("There was a problem with the '{0}' directive in your configuration: {1}")]
	Config(&'static str, Cow<'static, str>),
	#[error("{0}")]
	Conflict(Cow<'static, str>), // This is only needed for when a room alias already exists
	#[error(transparent)]
	ContentDisposition(#[from] ruma::http_headers::ContentDispositionParseError),
	#[error("{0}")]
	Database(Cow<'static, str>),
	#[error("Feature '{0}' is not available on this server.")]
	FeatureDisabled(Cow<'static, str>),
	#[error("Remote server {0} responded with: {1}")]
	Federation(ruma::OwnedServerName, ruma::api::client::error::Error),
	#[error("{0} in {1}")]
	InconsistentRoomState(&'static str, ruma::OwnedRoomId),
	#[error(transparent)]
	IntoHttp(#[from] ruma::api::error::IntoHttpError),
	#[error("{0}")]
	Ldap(Cow<'static, str>),
	#[error(transparent)]
	Mxc(#[from] ruma::MxcUriError),
	#[error(transparent)]
	Mxid(#[from] ruma::IdParseError),
	#[error(transparent)]
	PowerLevels(#[from] ruma::events::room::power_levels::PowerLevelsError),
	#[error("from {0}: {1}")]
	Redaction(ruma::OwnedServerName, ruma::canonical_json::RedactionError),
	#[error("{0}: {1}")]
	Request(ruma::api::client::error::ErrorKind, Cow<'static, str>, http::StatusCode),
	#[error(transparent)]
	Ruma(#[from] ruma::api::client::error::Error),
	#[error(transparent)]
	Signatures(#[from] ruma::signatures::Error),
	#[error("uiaa")]
	Uiaa(ruma::api::client::uiaa::UiaaInfo),

	// unique / untyped
	#[error("{0}")]
	Err(Cow<'static, str>),
}

static _IS_SEND: () = assert_send::<Error>();
static _IS_SYNC: () = assert_sync::<Error>();
static _IS_UNWIND_SAFE: () = assert_unwind_safe::<Error>();
static _IS_REF_UNWIND_SAFE: () = assert_ref_unwind_safe::<Error>();

impl Error {
	#[inline]
	#[must_use]
	pub fn from_errno() -> Self { Self::Io(std::io::Error::last_os_error()) }

	//#[deprecated]
	pub fn bad_database(message: &'static str) -> Self {
		crate::err!(Database(error!("{message}")))
	}

	/// Sanitizes public-facing errors that can leak sensitive information.
	pub fn sanitized_message(&self) -> String {
		match self {
			| Self::Database(..) => String::from("Database error occurred."),
			| Self::Io(..) => String::from("I/O error occurred."),
			| _ => self.message(),
		}
	}

	/// Generate the error message string.
	pub fn message(&self) -> String {
		match self {
			| Self::Federation(origin, error) => format!("Answer from {origin}: {error}"),
			| Self::Ruma(error) => response::ruma_error_message(error),
			| _ => format!("{self}"),
		}
	}

	/// Returns the Matrix error code / error kind
	#[inline]
	pub fn kind(&self) -> ruma::api::client::error::ErrorKind {
		use ruma::api::client::error::ErrorKind::{FeatureDisabled, NotJson, Unknown};

		match self {
			| Self::FeatureDisabled(..) => FeatureDisabled,
			| Self::CanonicalJson(..) | Self::Json(..) => NotJson,
			| Self::BadRequest(kind, ..) | Self::Request(kind, ..) => kind.clone(),
			| Self::Federation(_, error) | Self::Ruma(error) =>
				response::ruma_error_kind(error).clone(),
			| _ => Unknown,
		}
	}

	/// Returns the HTTP error code or closest approximation based on error
	/// variant.
	pub fn status_code(&self) -> http::StatusCode {
		use http::StatusCode;

		match self {
			| Self::Conflict(_) => StatusCode::CONFLICT, // room alias exists
			| Self::Federation(_, error) | Self::Ruma(error) => error.status_code,
			| Self::FeatureDisabled(..)
			| Self::CanonicalJson(..)
			| Self::Json(..)
			| Self::JsParseInt(..)
			| Self::JsTryFromInt(..) => response::bad_request_code(&self.kind()),
			| Self::BadRequest(kind, ..) => response::bad_request_code(kind),
			| Self::Request(kind, _, code) => response::status_code(kind, *code),
			| Self::Io(error) => response::io_error_code(error.kind()),
			| Self::Reqwest(error) => error
				.status()
				.unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
			| _ => StatusCode::INTERNAL_SERVER_ERROR,
		}
	}

	/// Returns true for "not found" errors. This means anything that qualifies
	/// as a "not found" from any variant's contained error type. This call is
	/// often used as a special case to eliminate a contained Option with a
	/// Result where Ok(None) is instead Err(e) if e.is_not_found().
	#[inline]
	pub fn is_not_found(&self) -> bool { self.status_code() == http::StatusCode::NOT_FOUND }
}

impl std::fmt::Debug for Error {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.message())
	}
}

impl<T> From<PoisonError<T>> for Error {
	#[cold]
	#[inline(never)]
	fn from(e: PoisonError<T>) -> Self { Self::Poison(e.to_string().into()) }
}

#[expect(clippy::fallible_impl_from)]
impl From<Infallible> for Error {
	#[cold]
	#[inline(never)]
	fn from(_e: Infallible) -> Self {
		panic!("infallible error should never exist");
	}
}

#[cold]
#[inline(never)]
pub fn infallible(_e: &Infallible) {
	panic!("infallible error should never exist");
}

/// Convenience functor for fundamental Error::sanitized_message(); see member.
#[inline]
#[must_use]
#[expect(clippy::needless_pass_by_value)]
pub fn sanitized_message(e: Error) -> String { e.sanitized_message() }
