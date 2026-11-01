//! Configures developer tracing tools.
//!
//! Folded span profiles support offline visualization. Tokio-console
//! supports runtime inspection.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

/// Developer tracing tools
///
/// Diagnostics for developers profiling the server. Neither `tokio_console`
/// nor `[global.tracing.flame]` does anything in the default build or the
/// published packages, so production servers can leave `[global.tracing]`
/// out.
///
/// Tips:
/// - `tokio_console` needs a build with the `tokio_console` Cargo feature
///   and `--cfg tokio_unstable` in `RUSTFLAGS`, and a release build must
///   also drop the default `release_max_log_level` feature. See
///   <https://tuwunel.chat/development.html#debugging-with-tokio-console>.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize, Default)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.tracing",
	group = "true",
	ignore = "flame"
)]
pub struct Tracing {
	/// Enables tokio-console for developer use as described in the
	/// [tokio-console debugging
	/// guide](https://tuwunel.chat/development.html#debugging-with-tokio-console).
	///
	/// legacy: tokio_console
	#[serde(default)]
	pub tokio_console: bool,

	/// Settings for tracing flame.
	///
	/// display: section
	#[serde(default)]
	pub flame: Flame,
}

/// Tracing span profiles (flame graphs)
///
/// Profiles time spent in the server's tracing spans and writes it to a
/// folded-stack file for flame-graph tools. Off by default, and it works only
/// in a build with the `perf_measurements` Cargo feature, which the default
/// build and the published packages leave out.
///
/// Tips:
/// - In such a build with `enable` on, an unwritable `output_path` or a
///   malformed `filter` stops startup.
/// - Output is buffered, so the file is complete only after the server stops.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.tracing.flame",
	group = "true"
)]
pub struct Flame {
	/// Collects folded stack profiles of tracing spans through tracing-flame
	/// when the `perf_measurements` build feature is enabled, for visualization
	/// with [inferno](https://github.com/jonhoo/inferno),
	/// [Speedscope](https://www.speedscope.app), or other compatible tools.
	///
	/// legacy: tracing_flame
	#[serde(default)]
	pub enable: bool,

	/// Selects tracing spans included in the folded stack profile.
	///
	/// default: "info"
	/// legacy: tracing_flame_filter
	#[serde(default = "default_filter")]
	pub filter: String,

	/// Selects the output path for the folded tracing-span profile.
	///
	/// default: "./tracing.folded"
	/// legacy: tracing_flame_output_path
	#[serde(default = "default_output_path")]
	pub output_path: String,
}

impl Default for Flame {
	fn default() -> Self {
		Self {
			enable: Default::default(),
			filter: default_filter(),
			output_path: default_output_path(),
		}
	}
}

fn default_filter() -> String {
	cfg!(debug_assertions)
		.then_some("trace,h2=off")
		.unwrap_or("info")
		.to_owned()
}

fn default_output_path() -> String { "./tracing.folded".to_owned() }
