//! Configures typing notifications.
//!
//! Federation delivery has separate incoming and outgoing controls. Local
//! and remote timeout bounds constrain typing indications.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::true_fn;

/// Configures typing notifications.
///
/// Federation delivery has separate incoming and outgoing controls. Local
/// and remote timeout bounds constrain typing indications.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.typing",
	group = "true"
)]
pub struct Typing {
	/// Allows sending typing updates over federation.
	///
	/// reloadable: yes
	/// legacy: allow_outgoing_typing
	#[serde(default = "true_fn")]
	pub allow_outgoing: bool,

	/// Allows receiving typing updates over federation.
	///
	/// reloadable: yes
	/// legacy: allow_incoming_typing
	#[serde(default = "true_fn")]
	pub allow_incoming: bool,

	/// Limits how long a federated user can indicate typing.
	///
	/// reloadable: yes
	/// default: 30
	/// legacy: typing_federation_timeout_s
	#[serde(default = "default_federation_timeout_s")]
	pub federation_timeout_s: u64,

	/// Sets the minimum time a local client can indicate typing.
	///
	/// A request to stop typing still takes effect. The minimum applies only
	/// when no stop request arrives.
	///
	/// reloadable: yes
	/// default: 15
	/// legacy: typing_client_timeout_min_s
	#[serde(default = "default_client_timeout_min_s")]
	pub client_timeout_min_s: u64,

	/// Limits how long a local client can indicate typing.
	///
	/// reloadable: yes
	/// default: 45
	/// legacy: typing_client_timeout_max_s
	#[serde(default = "default_client_timeout_max_s")]
	pub client_timeout_max_s: u64,
}

impl Default for Typing {
	fn default() -> Self {
		Self {
			allow_outgoing: true_fn(),
			allow_incoming: true_fn(),
			federation_timeout_s: default_federation_timeout_s(),
			client_timeout_min_s: default_client_timeout_min_s(),
			client_timeout_max_s: default_client_timeout_max_s(),
		}
	}
}

fn default_federation_timeout_s() -> u64 { 30 }

fn default_client_timeout_min_s() -> u64 { 15 }

fn default_client_timeout_max_s() -> u64 { 45 }
