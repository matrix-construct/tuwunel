//! Configures outgoing application-service transport.
//!
//! Request timeouts bound application-service calls. Idle connections have
//! a separate pool lifetime.

/// Outgoing application-service transport configuration.
pub use Client as AppserviceClient;
use serde::Deserialize;
use tuwunel_macros::config_example_generator;

/// Configures outgoing application-service transport.
///
/// Request timeouts bound application-service calls. Idle connections have
/// a separate pool lifetime.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.appservice_client",
	group = "true"
)]
pub struct Client {
	/// Sets appservice URL read and total timeouts in seconds, defaulting to
	/// 35 because appservices are generally hosted on the same network.
	///
	/// default: 35
	/// legacy: appservice_timeout
	#[serde(default = "default_timeout")]
	pub timeout: u64,

	/// Sets the appservice URL idle connection-pool timeout in seconds.
	///
	/// default: 300
	/// legacy: appservice_idle_timeout
	#[serde(default = "default_idle_timeout")]
	pub idle_timeout: u64,
}

impl Default for Client {
	fn default() -> Self {
		Self {
			timeout: default_timeout(),
			idle_timeout: default_idle_timeout(),
		}
	}
}

fn default_timeout() -> u64 { 35 }

fn default_idle_timeout() -> u64 { 300 }
