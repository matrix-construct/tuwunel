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

/// Unifies failures raised by the core crate.
///
/// Variants preserve typed causes where available and carry contextual text for
/// domain-specific failures. Conversion implementations allow callers to
/// propagate common dependency errors with `?`.
#[derive(thiserror::Error)]
pub enum Error {
	/// Carries an arbitrary panic payload.
	///
	/// The payload is protected by a mutex so the error remains shareable
	/// across unwind boundaries. Use the panic helpers to resume unwinding or
	/// inspect it.
	#[error("PANIC!")]
	PanicAny(Mutex<Box<dyn Any + Send>>),

	/// Carries a panic payload and its extracted static message.
	///
	/// The message supports diagnostics without consuming the payload. The
	/// mutex keeps the payload available across unwind boundaries.
	#[error("PANIC! {0}")]
	Panic(&'static str, Mutex<Box<dyn Any + Send + 'static>>),

	// std
	/// Reports a formatting failure.
	///
	/// Automatic conversion preserves the original source error. Its display
	/// text is forwarded unchanged.
	#[error(transparent)]
	Fmt(#[from] std::fmt::Error),

	/// Reports an invalid UTF-8 byte sequence while constructing a string.
	///
	/// Automatic conversion preserves the original bytes and source error. Its
	/// display text is forwarded unchanged.
	#[error(transparent)]
	FromUtf8(#[from] std::string::FromUtf8Error),

	/// Reports an input or output failure.
	///
	/// Automatic conversion preserves the original I/O error and its kind. HTTP
	/// response mapping may use the contained error kind.
	#[error("I/O error: {0}")]
	Io(#[from] std::io::Error),

	/// Reports a floating-point parsing failure.
	///
	/// Automatic conversion preserves the original source error. Its display
	/// text is forwarded unchanged.
	#[error(transparent)]
	ParseFloat(#[from] std::num::ParseFloatError),

	/// Reports an integer parsing failure.
	///
	/// Automatic conversion preserves the original source error. Its display
	/// text is forwarded unchanged.
	#[error(transparent)]
	ParseInt(#[from] std::num::ParseIntError),

	/// Carries a dynamically typed standard error.
	///
	/// The boxed source must be safe to send and share between threads. Its
	/// display text and source chain remain available for diagnostics.
	#[error(transparent)]
	Std(#[from] Box<dyn std::error::Error + Send + Sync + 'static>),

	/// Reports a system clock value earlier than the requested reference time.
	///
	/// Automatic conversion preserves the original source error. Its display
	/// text is forwarded unchanged.
	#[error(transparent)]
	SystemTime(#[from] std::time::SystemTimeError),

	/// Reports failure to access thread-local state.
	///
	/// Automatic conversion preserves the original source error. Its display
	/// text is forwarded unchanged.
	#[error(transparent)]
	ThreadAccessError(#[from] std::thread::AccessError),

	/// Reports an integer conversion outside the destination range.
	///
	/// Automatic conversion preserves the original source error. Its display
	/// text is forwarded unchanged.
	#[error(transparent)]
	TryFromInt(#[from] std::num::TryFromIntError),

	/// Reports conversion from a slice with an incompatible length.
	///
	/// Automatic conversion preserves the original source error. Its display
	/// text is forwarded unchanged.
	#[error(transparent)]
	TryFromSlice(#[from] std::array::TryFromSliceError),

	/// Reports an invalid borrowed UTF-8 byte sequence.
	///
	/// Automatic conversion preserves the original source error. Its display
	/// text is forwarded unchanged.
	#[error(transparent)]
	Utf8(#[from] std::str::Utf8Error),

	// third-party
	/// Reports that a fixed-capacity collection cannot accept another item.
	///
	/// Automatic conversion preserves the original capacity error. Its display
	/// text is forwarded unchanged.
	#[error(transparent)]
	CapacityError(#[from] arrayvec::CapacityError),

	/// Reports failure to parse Cargo manifest data.
	///
	/// Automatic conversion preserves the original source error. Its display
	/// text is forwarded unchanged.
	#[error(transparent)]
	CargoToml(#[from] cargo_toml::Error),

	/// Reports a command-line parsing or presentation failure.
	///
	/// Automatic conversion preserves the original Clap error. Its display text
	/// is forwarded unchanged.
	#[error(transparent)]
	Clap(#[from] clap::error::Error),

	/// Reports a Unix system error number.
	///
	/// Automatic conversion preserves the original error number. Its display
	/// text is forwarded unchanged.
	#[cfg(unix)]
	#[error(transparent)]
	Errno(#[from] nix::errno::Errno),

	/// Reports rejection of a required Axum request extension.
	///
	/// Automatic conversion preserves the extractor rejection. Its display text
	/// is forwarded unchanged.
	#[error(transparent)]
	Extension(#[from] axum::extract::rejection::ExtensionRejection),

	/// Reports a configuration extraction failure.
	///
	/// The boxed Figment error preserves its complete source and diagnostic
	/// context. A dedicated conversion boxes it for ordinary `?` propagation.
	#[error(transparent)]
	Figment(Box<figment::error::Error>),

	/// Reports failure to deserialize an HTML form.
	///
	/// Automatic conversion preserves the original source error. Its display
	/// text is forwarded unchanged.
	#[error(transparent)]
	HtmlFormDe(#[from] serde_html_form::de::Error),

	/// Reports failure to serialize an HTML form.
	///
	/// Automatic conversion preserves the original source error. Its display
	/// text is forwarded unchanged.
	#[error(transparent)]
	HtmlFormSer(#[from] serde_html_form::ser::Error),

	/// Reports failure to construct an HTTP value.
	///
	/// Automatic conversion preserves the original source error. Its display
	/// text is forwarded unchanged.
	#[error(transparent)]
	Http(#[from] http::Error),

	/// Reports an invalid HTTP header value.
	///
	/// Automatic conversion preserves the original source error. Its display
	/// text is forwarded unchanged.
	#[error(transparent)]
	HttpHeader(#[from] http::header::InvalidHeaderValue),

	/// Reports failure of a spawned asynchronous task.
	///
	/// The Tokio join error records cancellation and panic state. Panic helpers
	/// can recover its payload when the task panicked.
	#[error("Join error: {0}")]
	JoinError(#[from] tokio::task::JoinError),

	/// Reports failure to serialize or deserialize JSON.
	///
	/// Automatic conversion preserves the original source error. Matrix error
	/// mapping classifies this variant as invalid JSON.
	#[error(transparent)]
	Json(#[from] serde_json::Error),

	/// Reports failure to parse a Matrix-compatible JavaScript integer.
	///
	/// Automatic conversion preserves the re-exported integer error. Matrix
	/// response mapping treats it as a bad request.
	#[error(transparent)]
	JsParseInt(#[from] ruma::JsParseIntError), // js_int re-export

	/// Reports a value outside the Matrix JavaScript-integer range.
	///
	/// Automatic conversion preserves the re-exported conversion error. Matrix
	/// response mapping treats it as a bad request.
	#[error(transparent)]
	JsTryFromInt(#[from] ruma::JsTryFromIntError), // js_int re-export

	/// Reports an object-storage operation failure.
	///
	/// Automatic conversion preserves the backend source error. Its display
	/// text is forwarded unchanged.
	#[error(transparent)]
	ObjectStore(#[from] object_store::Error),

	/// Reports rejection of an Axum path parameter.
	///
	/// Automatic conversion preserves the extractor rejection. Its display text
	/// is forwarded unchanged.
	#[error(transparent)]
	Path(#[from] axum::extract::rejection::PathRejection),

	/// Reports access to a poisoned synchronization primitive.
	///
	/// The stored text reports the poisoning without retaining the guard.
	/// Poison conversion supplies the originating error text.
	#[error("Mutex poisoned: {0}")]
	Poison(Cow<'static, str>),

	/// Reports an invalid regular expression.
	///
	/// Automatic conversion preserves the original source error. The formatted
	/// message identifies the regex failure.
	#[error("Regex error: {0}")]
	Regex(#[from] regex::Error),

	/// Reports an HTTP client request failure.
	///
	/// Automatic conversion preserves status and transport details from
	/// Reqwest. HTTP response mapping reuses its status when one is available.
	#[error("Request error: {0}")]
	Reqwest(#[from] reqwest::Error),

	/// Reports a custom deserialization failure.
	///
	/// The message may borrow static text or own formatted context. It is
	/// exposed directly as the error display.
	#[error("{0}")]
	SerdeDe(Cow<'static, str>),

	/// Reports a custom serialization failure.
	///
	/// The message may borrow static text or own formatted context. It is
	/// exposed directly as the error display.
	#[error("{0}")]
	SerdeSer(Cow<'static, str>),

	/// Reports failure to deserialize TOML.
	///
	/// Automatic conversion preserves the original source error. Its display
	/// text is forwarded unchanged.
	#[error(transparent)]
	TomlDe(#[from] toml::de::Error),

	/// Reports failure to serialize TOML.
	///
	/// Automatic conversion preserves the original source error. Its display
	/// text is forwarded unchanged.
	#[error(transparent)]
	TomlSer(#[from] toml::ser::Error),

	/// Reports an invalid tracing filter directive.
	///
	/// Automatic conversion preserves the filter parser's source error. The
	/// formatted message identifies the tracing subsystem.
	#[error("Tracing filter error: {0}")]
	TracingFilter(#[from] tracing_subscriber::filter::ParseError),

	/// Reports failure to reload a tracing layer.
	///
	/// Automatic conversion preserves the reload source error. The formatted
	/// message identifies the tracing subsystem.
	#[error("Tracing reload error: {0}")]
	TracingReload(#[from] tracing_subscriber::reload::Error),

	/// Reports rejection of a typed HTTP header.
	///
	/// Automatic conversion preserves the extractor rejection. Its display text
	/// is forwarded unchanged.
	#[error(transparent)]
	TypedHeader(#[from] axum_extra::typed_header::TypedHeaderRejection),

	/// Reports failure to parse a URL.
	///
	/// Automatic conversion preserves the original source error. Its display
	/// text is forwarded unchanged.
	#[error(transparent)]
	UrlParse(#[from] url::ParseError),

	/// Reports failure to serialize or deserialize YAML.
	///
	/// Automatic conversion preserves the original source error. Its display
	/// text is forwarded unchanged.
	#[error(transparent)]
	Yaml(#[from] serde_yaml::Error),

	// ruma/tuwunel
	/// Reports an arithmetic operation that cannot produce a valid result.
	///
	/// The message records contextual conversion, range, overflow, and
	/// underflow failures. It can supplement lower-level typed numeric source
	/// errors.
	#[error("Arithmetic operation failed: {0}")]
	Arithmetic(Cow<'static, str>),

	/// State-res `auth_check` rejection sentinel.
	///
	/// Surfaces to the wire as 403 / M_FORBIDDEN with the Display text
	/// `Auth check failed: {inner}`. Exists so callers can pattern-match the
	/// cause without grepping the message text.
	#[error("Auth check failed: {0}")]
	AuthCheck(Box<Self>),

	/// Reports a legacy structured Matrix request error.
	///
	/// The variant pairs a Matrix error kind with static public text. Response
	/// mapping derives the appropriate HTTP status from that kind.
	#[error("{0}: {1}")]
	BadRequest(ruma::api::error::ErrorKind, &'static str), //TODO: remove

	/// Reports an invalid or unusable response from a remote server.
	///
	/// The message carries protocol context suitable for diagnostics. No typed
	/// remote response is retained.
	#[error("{0}")]
	BadServerResponse(Cow<'static, str>),

	/// Reports invalid canonical JSON.
	///
	/// Automatic conversion preserves the original canonicalization error.
	/// Matrix error mapping classifies this variant as invalid JSON.
	#[error(transparent)]
	CanonicalJson(#[from] ruma::CanonicalJsonError),

	/// Reports an invalid configuration directive.
	///
	/// The static directive name identifies the setting and the accompanying
	/// message explains why its value cannot be used.
	#[error("There was a problem with the '{0}' directive in your configuration: {1}")]
	Config(&'static str, Cow<'static, str>),

	/// Reports a resource conflict.
	///
	/// This variant currently represents an already occupied room alias. HTTP
	/// response mapping emits a conflict status.
	#[error("{0}")]
	Conflict(Cow<'static, str>), // This is only needed for when a room alias already exists

	/// Reports an invalid Matrix content-disposition header.
	///
	/// Automatic conversion preserves the original parser error. Its display
	/// text is forwarded unchanged.
	#[error(transparent)]
	ContentDisposition(#[from] ruma::http_headers::ContentDispositionParseError),

	/// Reports a database operation or invariant failure.
	///
	/// The message carries storage context without exposing it in sanitized
	/// client-facing output. Database failures default to an internal status.
	#[error("{0}")]
	Database(Cow<'static, str>),

	/// Reports use of a feature disabled by server configuration.
	///
	/// The stored feature name is included in the public message. Matrix
	/// response mapping assigns the matching feature-disabled error kind.
	#[error("Feature '{0}' is not available on this server.")]
	FeatureDisabled(Cow<'static, str>),

	/// Reports an error response received from a federated server.
	///
	/// The variant preserves both the origin and its structured Matrix error.
	/// Response mapping forwards the remote status and error kind.
	#[error("Remote server {0} responded with: {1}")]
	Federation(ruma::OwnedServerName, ruma::api::error::Error),

	/// Carries a preconstructed HTTP status and JSON response body.
	///
	/// Response mapping preserves the caller-selected status. The formatted
	/// JSON becomes the message of a standard Matrix error response.
	#[error("{0}: {1:#?}")]
	HttpJson(http::StatusCode, axum::Json<serde_json::Value>),

	/// Reports an invariant violation in a room's persisted state.
	///
	/// The static message names the failed invariant and the room identifier
	/// locates the affected state.
	#[error("{0} in {1}")]
	InconsistentRoomState(&'static str, ruma::OwnedRoomId),

	/// Reports failure to convert a Matrix response into HTTP form.
	///
	/// Automatic conversion preserves the original Ruma source error. Its
	/// display text is forwarded unchanged.
	#[error(transparent)]
	IntoHttp(#[from] ruma::api::error::IntoHttpError),

	/// Reports an LDAP operation failure.
	///
	/// The message records directory-service context not represented by a
	/// common typed source. Response mapping treats it as an internal error.
	#[error("{0}")]
	Ldap(Cow<'static, str>),

	/// Reports an invalid Matrix content URI.
	///
	/// Automatic conversion preserves the original URI error. Its display text
	/// is forwarded unchanged.
	#[error(transparent)]
	Mxc(#[from] ruma::MxcUriError),

	/// Reports an invalid Matrix identifier.
	///
	/// Automatic conversion preserves the original identifier parser error. Its
	/// display text is forwarded unchanged.
	#[error(transparent)]
	Mxid(#[from] ruma::IdParseError),

	/// Reports invalid room power-level content or arithmetic.
	///
	/// Automatic conversion preserves the original Ruma power-level error. Its
	/// display text is forwarded unchanged.
	#[error(transparent)]
	PowerLevels(#[from] ruma::events::room::power_levels::PowerLevelsError),

	/// Reports failure to redact canonical JSON from a remote server.
	///
	/// The variant records the origin alongside the invalid canonical field.
	/// The formatted message keeps both pieces of context.
	#[error("from {0}: {1}")]
	Redaction(ruma::OwnedServerName, ruma::canonical_json::CanonicalJsonFieldError),

	/// Carries a structured Matrix client error response.
	///
	/// The variant stores the Matrix error kind, public message, and preferred
	/// HTTP status. Response mapping may refine the status from the kind.
	#[error("{0}: {1}")]
	Request(ruma::api::error::ErrorKind, Cow<'static, str>, http::StatusCode),

	/// Reports a structured Matrix API error.
	///
	/// Automatic conversion preserves the Ruma status, kind, and message.
	/// Response mapping forwards those structured fields.
	#[error(transparent)]
	Ruma(#[from] ruma::api::error::Error),

	/// Reports a Matrix signature verification failure.
	///
	/// Automatic conversion preserves the original verification error. Its
	/// display text is forwarded unchanged.
	#[error(transparent)]
	Signatures(#[from] ruma::signatures::VerificationError),

	/// Reports invalid JSON encountered during signature processing.
	///
	/// Automatic conversion preserves the signature library's JSON error. Its
	/// display text is forwarded unchanged.
	#[error(transparent)]
	SignaturesJson(#[from] ruma::signatures::JsonError),

	/// Requests an interactive-authentication challenge response.
	///
	/// The contained UIAA information is serialized for the client rather than
	/// treated as an opaque internal failure.
	#[error("uiaa")]
	Uiaa(ruma::api::client::uiaa::UiaaInfo),

	// unique / untyped
	/// Reports an untyped core failure.
	///
	/// The message may borrow static text or own formatted context. This
	/// fallback is used when no structured variant represents the failure.
	#[error("{0}")]
	Err(Cow<'static, str>),
}

static _IS_SEND: () = assert_send::<Error>();
static _IS_SYNC: () = assert_sync::<Error>();
static _IS_UNWIND_SAFE: () = assert_unwind_safe::<Error>();
static _IS_REF_UNWIND_SAFE: () = assert_ref_unwind_safe::<Error>();

impl Error {
	/// Captures the operating system's most recent error for the current
	/// thread.
	///
	/// The error is sampled when this function is called and wrapped as
	/// [`Error::Io`]. Platform-specific code remains available through the
	/// source.
	#[inline]
	#[must_use]
	pub fn from_errno() -> Self { Self::Io(std::io::Error::last_os_error()) }

	/// Constructs a database error from static diagnostic text.
	///
	/// The error helper records the call site while preserving the supplied
	/// message. Callers exposing it publicly can use
	/// [`Error::sanitized_message`].
	//#[deprecated]
	pub fn bad_database(message: &'static str) -> Self {
		crate::err!(Database(error!("{message}")))
	}

	/// Produces an error message safe for public responses.
	///
	/// Database and I/O details are replaced with generic text to avoid leaking
	/// sensitive context. Other variants retain their normal message.
	pub fn sanitized_message(&self) -> String {
		match self {
			| Self::Database(..) => String::from("Database error occurred."),
			| Self::Io(..) => String::from("I/O error occurred."),
			| _ => self.message(),
		}
	}

	/// Formats the diagnostic message for this error.
	///
	/// Federation errors include their origin and Ruma errors use their Matrix
	/// response message. Other variants use their
	/// [`Display`](std::fmt::Display) implementation.
	pub fn message(&self) -> String {
		match self {
			| Self::Federation(origin, error) => format!("Answer from {origin}: {error}"),
			| Self::Ruma(error) => response::ruma_error_message(error),
			| _ => format!("{self}"),
		}
	}

	/// Returns the Matrix error kind represented by this error.
	///
	/// Structured request and federation variants preserve their supplied kind.
	/// Unclassified internal errors map to `M_UNKNOWN`.
	#[inline]
	pub fn kind(&self) -> ruma::api::error::ErrorKind {
		use ruma::api::error::{
			ErrorKind,
			ErrorKind::{FeatureDisabled, NotJson, Unknown},
		};

		match self {
			| Self::FeatureDisabled(..) => FeatureDisabled,
			| Self::CanonicalJson(..) | Self::Json(..) => NotJson,
			| Self::AuthCheck(..) => ErrorKind::forbidden(),
			| Self::BadRequest(kind, ..) | Self::Request(kind, ..) => kind.clone(),
			| Self::Federation(_, error) | Self::Ruma(error) =>
				response::ruma_error_kind(error).clone(),
			| _ => Unknown,
		}
	}

	/// Returns the HTTP status represented by this error.
	///
	/// Structured variants preserve or derive their protocol status, while I/O
	/// and client errors use their available status metadata. Unclassified
	/// failures map to an internal-server-error status.
	pub fn status_code(&self) -> http::StatusCode {
		use http::StatusCode;

		match self {
			| Self::AuthCheck(..) => StatusCode::FORBIDDEN,
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
			| Self::HttpJson(code, ..) => *code,
			| Self::Reqwest(error) => error
				.status()
				.unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
			| _ => StatusCode::INTERNAL_SERVER_ERROR,
		}
	}

	/// Tests whether this error maps to an HTTP not-found status.
	///
	/// The test includes contained error types whose status mapping yields 404.
	/// Callers can use it to treat `Err` as the absent case in place of a
	/// nested `Option`.
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

impl From<figment::error::Error> for Error {
	#[cold]
	#[inline(never)]
	fn from(e: figment::error::Error) -> Self { Self::Figment(Box::new(e)) }
}

#[expect(clippy::fallible_impl_from)]
impl From<Infallible> for Error {
	#[cold]
	#[inline(never)]
	fn from(_e: Infallible) -> Self {
		panic!("infallible error should never exist");
	}
}

/// Marks an impossible [`Infallible`] error path.
///
/// The argument cannot be constructed in safe code, so reaching this function
/// indicates a violated invariant.
///
/// # Panics
///
/// Always panics because an `Infallible` error cannot legitimately exist.
#[cold]
#[inline(never)]
pub fn infallible(_e: &Infallible) {
	panic!("infallible error should never exist");
}

/// Produces a public-safe message from an owned error.
///
/// Its by-value signature adapts [`Error::sanitized_message`] for iterator and
/// future combinators that consume their item. Sanitization behavior is
/// identical to the method.
#[inline]
#[must_use]
#[expect(clippy::needless_pass_by_value)]
pub fn sanitized_message(e: Error) -> String { e.sanitized_message() }
