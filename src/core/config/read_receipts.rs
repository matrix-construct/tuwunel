//! Configures federated read receipts.
//!
//! Incoming and outgoing receipt delivery have separate controls.

/// Federated read-receipt configuration.
pub use Config as ReadReceipts;
use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::true_fn;

/// Configures federated read receipts.
///
/// Incoming and outgoing receipt delivery have separate controls.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.read_receipts",
	group = "true"
)]
pub struct Config {
	/// Allows receiving read receipts from remote servers.
	///
	/// reloadable: yes
	/// legacy: allow_incoming_read_receipts
	#[serde(default = "true_fn")]
	pub allow_incoming: bool,

	/// Allows sending read receipts to remote servers.
	///
	/// reloadable: yes
	/// legacy: allow_outgoing_read_receipts
	#[serde(default = "true_fn")]
	pub allow_outgoing: bool,
}

impl Default for Config {
	fn default() -> Self {
		Self {
			allow_incoming: true_fn(),
			allow_outgoing: true_fn(),
		}
	}
}
