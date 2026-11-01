//! Configures loading and reloading policy.
//!
//! Unknown-option handling selects warnings or startup errors. The signal
//! switch controls configuration reload requests.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::true_fn;

/// Configuration loading and reload
///
/// Controls how strictly the configuration is checked and whether `SIGUSR1`
/// reloads it. By default an unknown key logs a warning and is ignored, and
/// `SIGUSR1` (what `systemctl reload` sends with the packaged service units)
/// re-reads every configuration source, applying the keys marked
/// `reloadable: yes`.
///
/// Tips:
/// - A reload runs the same checks as startup. If the new configuration
///   fails them, the server keeps the running one and logs why.
///   `server_name` and `http.ip_source` cannot change on reload.
/// - `reload_signal` gates only the signal; the admin command
///   `!admin server reload-config` reloads regardless.
/// - Guide: <https://tuwunel.chat/deploying/configuration-reload.html>.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.configuration",
	group = "true"
)]
pub struct Configuration {
	/// Rejects startup when an unknown configuration option is specified,
	/// whereas the default false warns to allow future deprecation or removal
	/// of options without breaking existing deployments.
	///
	/// reloadable: yes
	/// legacy: error_on_unknown_config_opts
	#[serde(default)]
	pub error_on_unknown_opts: bool,

	/// Reloads configuration on SIGUSR1 on supporting platforms when enabled.
	///
	/// reloadable: yes
	/// default: true
	/// legacy: config_reload_signal
	#[serde(default = "true_fn")]
	pub reload_signal: bool,
}

impl Default for Configuration {
	fn default() -> Self {
		Self {
			error_on_unknown_opts: Default::default(),
			reload_signal: true_fn(),
		}
	}
}
