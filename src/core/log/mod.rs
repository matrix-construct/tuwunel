/// Ephemeral tracing-event capture.
///
/// Captures combine optional predicates with callbacks and remain active for a
/// scope guard's lifetime. Formatting helpers support administrative output.
pub mod capture;
/// HTML color values for log levels.
///
/// Helpers map tracing metadata to hexadecimal colors for Matrix-formatted log
/// output. HTML and code-tag capture formatters share these values.
pub mod color;
/// Console formatting and output routing.
///
/// The module selects stdout, stderr, or native journal output and formats each
/// event according to logging configuration.
pub mod console;
/// HTML and Markdown formatting for captured log events.
///
/// Each helper appends one entry or table header to a caller-provided
/// formatter. Event entries include tracing level and span context, while the
/// header helper writes column labels.
pub mod fmt;
/// Parsing for tracing span-lifecycle formatting modes.
///
/// Known text values map case-insensitively to `FmtSpan` flags. Unknown values
/// return `FmtSpan::NONE` in the error variant.
pub mod fmt_span;
/// Native systemd journal submission and field recording.
///
/// The module routes formatted messages to the journal socket and records
/// structured tracing fields for journal queries.
pub mod journald;
mod reload;
mod suppress;

use std::sync::Arc;

pub use tracing::{Level, subscriber::Subscriber};
pub use tracing_core::{Event, Metadata};
pub use tracing_subscriber::EnvFilter;

pub use self::{
	capture::Capture,
	console::{ConsoleFormat, ConsoleWriter, ansi_enabled, is_systemd_mode, is_terminal_mode},
	reload::{LogLevelReloadHandles, ReloadHandle},
	suppress::Suppress,
};

/// Owns the server's shared logging and tracing state.
///
/// The singleton groups the configured subscriber, reload handles, and
/// ephemeral capture registry. `Server` exposes this value to components that
/// adjust logging.
pub struct Logging {
	/// Retains the subscriber configured for the server.
	///
	/// Initialization may register it globally when `log_global_default` is
	/// enabled. Otherwise no global registration occurs, and disabled logging
	/// can retain a no-op subscriber.
	pub subscriber: Arc<dyn Subscriber + Send + Sync>,

	/// Named handles for changing subscriber filters at runtime.
	///
	/// Administrative commands and scoped guards use these handles without
	/// knowing the concrete subscriber stack type.
	pub reload: LogLevelReloadHandles,

	/// Shared registration state for ephemeral tracing captures.
	///
	/// Capture guards add and remove callbacks observed by the capture layer.
	pub capture: Arc<capture::State>,
}

// Wraps for logging macros. Use these macros rather than extern tracing:: or
// log:: crates in project code. ::log and ::tracing can still be used if
// necessary but discouraged. Remember debug_ log macros are also exported to
// the crate namespace like these.

/// Emits a tracing event at a caller-supplied level.
///
/// The macro forwards tracing fields and message tokens to `tracing::event!`.
/// Project code can use it without importing the tracing crate directly.
#[macro_export]
#[collapse_debuginfo(yes)]
macro_rules! event {
	( $level:expr_2021, $($x:tt)+ ) => { ::tracing::event!( $level, $($x)+ ) }
}

/// Emits an error-level tracing event.
///
/// The macro forwards all fields and message tokens to `tracing::error!`.
/// Project code can use it without importing the tracing crate directly.
#[macro_export]
macro_rules! error {
    ( $($x:tt)+ ) => { ::tracing::error!( $($x)+ ) }
}

/// Emits a warning-level tracing event.
///
/// The macro forwards all fields and message tokens to `tracing::warn!`.
/// Project code can use it without importing the tracing crate directly.
#[macro_export]
macro_rules! warn {
    ( $($x:tt)+ ) => { ::tracing::warn!( $($x)+ ) }
}

/// Emits an informational tracing event.
///
/// The macro forwards all fields and message tokens to `tracing::info!`.
/// Project code can use it without importing the tracing crate directly.
#[macro_export]
macro_rules! info {
    ( $($x:tt)+ ) => { ::tracing::info!( $($x)+ ) }
}

/// Emits a debug-level tracing event.
///
/// The macro forwards all fields and message tokens to `tracing::debug!`.
/// Project code can use it without importing the tracing crate directly.
#[macro_export]
macro_rules! debug {
    ( $($x:tt)+ ) => { ::tracing::debug!( $($x)+ ) }
}

/// Emits a trace-level tracing event.
///
/// The macro forwards all fields and message tokens to `tracing::trace!`.
/// Project code can use it without importing the tracing crate directly.
#[macro_export]
macro_rules! trace {
    ( $($x:tt)+ ) => { ::tracing::trace!( $($x)+ ) }
}
