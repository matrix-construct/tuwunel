pub mod capture;
pub mod color;
pub mod console;
pub mod fmt;
pub mod fmt_span;
mod reload;
mod suppress;

use std::sync::Arc;

pub use tracing::{Level, subscriber::Subscriber};
pub use tracing_core::{Event, Metadata};
pub use tracing_subscriber::EnvFilter;

pub use self::{
	capture::Capture,
	console::{ConsoleFormat, ConsoleWriter, is_systemd_mode},
	reload::{LogLevelReloadHandles, ReloadHandle},
	suppress::Suppress,
};

/// Logging subsystem. This is a singleton member of super::Server which holds
/// all logging and tracing related state rather than shoving it all in
/// super::Server directly.
pub struct Logging {
	/// Subscriber assigned to globals and defaults; may also be NoSubscriber.
	pub subscriber: Arc<dyn Subscriber + Send + Sync>,

	/// General log level reload handles.
	pub reload: LogLevelReloadHandles,

	/// Tracing capture state for ephemeral/oneshot uses.
	pub capture: Arc<capture::State>,
}

// Wraps for logging macros. Use these macros rather than extern tracing:: or
// log:: crates in project code. ::log and ::tracing can still be used if
// necessary but discouraged. Remember debug_ log macros are also exported to
// the crate namespace like these.

#[macro_export]
#[collapse_debuginfo(yes)]
macro_rules! event {
	( $level:expr_2021, $($x:tt)+ ) => { ::tracing::event!( $level, $($x)+ ) }
}

#[macro_export]
macro_rules! error {
    ( $($x:tt)+ ) => { ::tracing::error!( $($x)+ ) }
}

#[macro_export]
macro_rules! warn {
    ( $($x:tt)+ ) => { ::tracing::warn!( $($x)+ ) }
}

#[macro_export]
macro_rules! info {
    ( $($x:tt)+ ) => { ::tracing::info!( $($x)+ ) }
}

#[macro_export]
macro_rules! debug {
    ( $($x:tt)+ ) => { ::tracing::debug!( $($x)+ ) }
}

#[macro_export]
macro_rules! trace {
    ( $($x:tt)+ ) => { ::tracing::trace!( $($x)+ ) }
}
