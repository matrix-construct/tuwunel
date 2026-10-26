//! Configures log filtering, formatting, and output.
//!
//! Console and journal settings select destinations and presentation.
//! Low-level initialization controls support developer and expert use.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::true_fn;

/// Configures log filtering, formatting, and output.
///
/// Console and journal settings select destinations and presentation.
/// Low-level initialization controls support developer and expert use.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.logging",
	group = "true"
)]
pub struct Logging {
	/// Sets the maximum log level, accepting debug, info, warn, or error.
	///
	/// The filter also accepts [tracing-subscriber
	/// directives](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html#directives).
	/// Release builds with `release_max_log_level` omit debug and trace levels
	/// to reduce compiled tracing overhead; debug builds do not apply that
	/// release cap.
	///
	/// default: "info"
	/// legacy: log
	#[serde(default = "default_filter")]
	pub filter: String,

	/// Adds ANSI colors to log output.
	///
	/// Colors are suppressed for journald submissions because it treats
	/// control bytes in formatted lines as binary instead of text.
	///
	/// legacy: log_colors log_colours
	#[serde(default = "true_fn", alias = "log_colours")]
	pub colors: bool,

	/// Selects the compact log format.
	///
	/// legacy: log_compact
	#[serde(default)]
	pub compact: bool,

	/// Selects span events included in log output.
	///
	/// default: "none"
	/// legacy: log_span_events
	#[serde(default = "default_span_events")]
	pub span_events: String,

	/// Allows the log EnvFilter to match values using regular expressions.
	///
	/// This controls `TUWUNEL_LOG` filter matching; see the
	/// [tracing-subscriber directives
	/// documentation](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html#directives).
	///
	/// default: true
	/// legacy: log_filter_regex
	#[serde(default = "true_fn")]
	pub filter_regex: bool,

	/// Displays thread IDs in tracing log output.
	///
	/// default: false
	/// legacy: log_thread_ids
	#[serde(default)]
	pub thread_ids: bool,

	/// Redirects logging to standard error.
	///
	/// The default false writes to standard output. Systemd integration
	/// redirects to standard error as needed without this setting; this option
	/// serves other deployments requiring redirection.
	///
	/// default: false
	/// legacy: log_to_stderr
	#[serde(default)]
	pub to_stderr: bool,

	/// Submits logs directly to journald instead of the console when running
	/// under systemd.
	///
	/// Each entry carries its real severity as journal priority, so
	/// `journalctl -p warning` finds warnings and errors; console capture
	/// instead assigns one fixed priority. Messages retain console formatting
	/// and span fields, with target, source location, and all tracing fields
	/// attached as journal fields, the latter prefixed `F_` for queries such
	/// as `journalctl F_ROOM_ID='!room:example.com'`. Outside systemd this has
	/// no effect, and failure to open the journald socket retains console
	/// output.
	///
	/// default: true
	/// legacy: log_journald
	#[serde(default = "true_fn")]
	pub journald: bool,

	/// Initializes the logging and tracing system when enabled.
	///
	/// When false the system is neither initialized nor operating, whereas an
	/// empty `logging.filter` leaves it operating but muted. Changes have no
	/// effect after startup. This is for developers and expert use; prefer an
	/// empty filter to disabling the system.
	///
	/// default: true
	/// legacy: log_enable
	#[serde(default = "true_fn")]
	pub enable: bool,

	/// Registers the configured logging and tracing system as the global
	/// handler.
	///
	/// Disabling this is a low-level developer or expert setting similar to
	/// `logging.enable`, but the system is configured normally and only the
	/// final global registration is omitted.
	///
	/// default: true
	/// legacy: log_global_default
	#[serde(default = "true_fn")]
	pub global_default: bool,
}

impl Default for Logging {
	fn default() -> Self {
		Self {
			filter: default_filter(),
			colors: true_fn(),
			compact: Default::default(),
			span_events: default_span_events(),
			filter_regex: true_fn(),
			thread_ids: Default::default(),
			to_stderr: Default::default(),
			journald: true_fn(),
			enable: true_fn(),
			global_default: true_fn(),
		}
	}
}

/// Returns the default log filter.
///
/// Debug builds use `debug`; other builds use `info`.
#[must_use]
pub fn default_filter() -> String {
	cfg!(debug_assertions)
		.then_some("debug")
		.unwrap_or("info")
		.to_owned()
}

/// Returns the default tracing span-event mode.
///
/// The value is `none`, which disables span lifecycle event emission. It is
/// used when `logging.span_events` is omitted.
#[must_use]
pub fn default_span_events() -> String { "none".into() }
