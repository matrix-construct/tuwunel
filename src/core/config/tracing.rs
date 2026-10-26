//! Configures developer tracing tools.
//!
//! Folded span profiles support offline visualization. Tokio-console
//! supports runtime inspection.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

/// Configures developer tracing tools.
///
/// Folded span profiles support offline visualization. Tokio-console
/// supports runtime inspection.
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

/// Configures folded tracing-span profiles.
///
/// A filter selects collected spans. The output path names the resulting
/// profile file.
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
