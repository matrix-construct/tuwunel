//! Configures optional external telemetry.
//!
//! Sentry handles crash reports and performance sampling. Jaeger-compatible
//! collectors receive selected tracing spans.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::{Url, true_fn};

/// External telemetry
///
/// Sends diagnostics to services outside the server. Both exporters are off
/// by default.
///
/// - `[global.telemetry.sentry]`: crash, error and performance reports to a
///   Sentry service. Included in the default build.
/// - `[global.telemetry.jaeger]`: tracing spans to an OpenTelemetry (OTLP)
///   collector such as Jaeger. Needs a build with the `perf_measurements`
///   Cargo feature.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
// No Debug: secrets are reachable from this section.
#[derive(Clone, Deserialize, Default)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.telemetry",
	group = "true",
	ignore = "jaeger sentry"
)]
pub struct Telemetry {
	/// Settings for telemetry jaeger.
	///
	/// display: section
	#[serde(default)]
	pub jaeger: Jaeger,

	/// Settings for telemetry sentry.
	///
	/// display: section
	#[serde(default)]
	pub sentry: Sentry,
}

/// Jaeger span export (OpenTelemetry)
///
/// Exports tracing spans over OTLP (gRPC) to an OpenTelemetry collector such
/// as Jaeger. Off by default, and available only in a build with the
/// `perf_measurements` Cargo feature; the default build and the published
/// packages ignore these keys.
///
/// Tips:
/// - There is no key for the collector address. The exporter reads the
///   standard `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` environment variable (or
///   `OTEL_EXPORTER_OTLP_ENDPOINT`) and otherwise sends to
///   `http://localhost:4317`.
/// - Spans arrive under the service name `tuwunel`.
/// - In a `perf_measurements` build a malformed `filter` stops startup even
///   with `enable` off.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.telemetry.jaeger",
	group = "true"
)]
pub struct Jaeger {
	/// Enables OTLP span export for Jaeger-compatible tracing.
	///
	/// A build with performance measurements installs an OpenTelemetry layer
	/// when enabled. The default is false, and `telemetry.jaeger.filter`
	/// selects exported spans.
	///
	/// legacy: allow_jaeger
	#[serde(default)]
	pub enable: bool,

	/// Selects tracing spans exported to Jaeger-compatible collectors.
	///
	/// default: "info"
	/// legacy: jaeger_filter
	#[serde(default = "default_filter")]
	pub filter: String,
}

/// Sentry error reporting
///
/// Reports panics, error-level log events and a sampled share of performance
/// traces to a Sentry service. Support is in the default build, but `enable`
/// is off, so nothing is sent until you turn it on. Settings are read once at
/// startup; a configuration reload neither starts nor stops reporting.
///
/// Set `enable = true` to report. `endpoint` takes a Sentry DSN; the default
/// DSN reports to the tuwunel project's Sentry
/// (`o4509498990067712.ingest.us.sentry.io`). Set your own DSN to keep
/// reports in your Sentry.
///
/// Tips:
/// - Error events carry the log message and its fields, with recent `info`
///   and `warn` log lines attached as breadcrumbs, so reports can include
///   user IDs and room IDs.
/// - Sentry fills its own `server_name` field with the machine's hostname
///   unless `send_server_name` replaces it with the Matrix server name. OS
///   and Rust version details are attached to events as well.
/// - With `enable` on, a malformed `filter` stops startup.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
// No Debug: secrets are reachable from this section.
#[derive(Clone, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.telemetry.sentry",
	group = "true"
)]
pub struct Sentry {
	/// Enables Sentry crash and panic reporting, performance monitoring, and
	/// metrics, disabled at runtime by default and sent to the default endpoint
	/// domain `o4509498990067712.ingest.us.sentry.io` unless overridden.
	///
	/// legacy: sentry
	#[serde(default)]
	pub enable: bool,

	/// Selects a custom Sentry reporting URL.
	///
	/// display: sensitive
	/// default: ""
	/// legacy: sentry_endpoint
	#[serde(default = "default_endpoint")]
	pub endpoint: Option<Url>,

	/// Includes `server_name` in Sentry crash reports and metrics.
	///
	/// legacy: sentry_send_server_name
	#[serde(default)]
	pub send_server_name: bool,

	/// Sets the fraction of performance-monitoring traces sampled by Sentry.
	///
	/// When Sentry is enabled, this must be between 0.0 and 1.0 inclusive.
	/// High values may affect performance; 0.0 disables sampling.
	///
	/// default: 0.15
	/// legacy: sentry_traces_sample_rate
	#[serde(default = "default_traces_sample_rate")]
	pub traces_sample_rate: f32,

	/// Attaches a stack trace to Sentry reports.
	///
	/// legacy: sentry_attach_stacktrace
	#[serde(default)]
	pub attach_stacktrace: bool,

	/// Sends panics to Sentry when telemetry is enabled.
	///
	/// This defaults to true, but `telemetry.sentry.enable` must also be
	/// enabled before any data is sent.
	///
	/// legacy: sentry_send_panic
	#[serde(default = "true_fn")]
	pub send_panic: bool,

	/// Sends errors to Sentry when telemetry is enabled.
	///
	/// This defaults to true but requires Sentry enablement. It applies only
	/// in release mode and is forced false in debug mode.
	///
	/// legacy: sentry_send_error
	#[serde(default = "true_fn")]
	pub send_error: bool,

	/// Selects the tracing log level for Sentry breadcrumbs and transactions.
	///
	/// default: "info"
	/// legacy: sentry_filter
	#[serde(default = "default_sentry_filter")]
	pub filter: String,
}

impl Default for Jaeger {
	fn default() -> Self {
		Self {
			enable: Default::default(),
			filter: default_filter(),
		}
	}
}

impl Default for Sentry {
	fn default() -> Self {
		Self {
			enable: Default::default(),
			endpoint: default_endpoint(),
			send_server_name: Default::default(),
			traces_sample_rate: default_traces_sample_rate(),
			attach_stacktrace: Default::default(),
			send_panic: true_fn(),
			send_error: true_fn(),
			filter: default_sentry_filter(),
		}
	}
}

fn default_filter() -> String {
	cfg!(debug_assertions)
		.then_some("trace,h2=off")
		.unwrap_or("info")
		.to_owned()
}

fn default_endpoint() -> Option<Url> {
	Url::try_from("https://8994b1762a6a95af9502a7900edabc4c@o4509498990067712.ingest.us.sentry.io/4509498993213440")
		.expect("default sentry url is invalid")
		.into()
}

fn default_traces_sample_rate() -> f32 { 0.15 }

fn default_sentry_filter() -> String { "info".to_owned() }
