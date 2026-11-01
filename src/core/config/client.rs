//! Configures client-facing protocol behavior.
//!
//! Sync settings bound long-polling timeouts. Room-summary controls select
//! sync calculations.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::true_fn;

/// Client API
///
/// Groups settings for how the server answers client requests.
/// `[global.client.sync]` bounds sync long polling and turns room hero
/// calculation on or off.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize, Default)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.client",
	group = "true",
	ignore = "sync"
)]
pub struct Client {
	/// Settings for client sync.
	///
	/// display: section
	#[serde(default)]
	pub sync: Sync,
}

/// Client sync long polling
///
/// Bounds how long a client's sync request may wait for new events before
/// the server answers anyway, and whether sync computes room heroes.
///
/// Tips:
/// - Sliding sync honors only `timeout_max`; `timeout_min` and
///   `timeout_default` apply to legacy `/sync` and to `/events`.
/// - Keep `timeout_max` (milliseconds) below `request_timeout` in
///   `[global.http]` (seconds) and below the read timeout of any reverse
///   proxy in front of the server, or long polls fail instead of returning.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.client.sync",
	group = "true"
)]
pub struct Sync {
	/// Sets the minimum requested timeout for long-polling sync.
	///
	/// Smaller client requests are clamped up to this value.
	///
	/// reloadable: yes
	/// default: 5000
	/// legacy: client_sync_timeout_min
	#[serde(default = "default_timeout_min")]
	pub timeout_min: u64,

	/// Sets the long-polling sync timeout when the client supplies none in its
	/// query string.
	///
	/// reloadable: yes
	/// default: 30000
	/// legacy: client_sync_timeout_default
	#[serde(default = "default_timeout_default")]
	pub timeout_default: u64,

	/// Sets the maximum requested timeout for long-polling sync.
	///
	/// Larger client requests are clamped down to this value.
	///
	/// reloadable: yes
	/// default: 90000
	/// legacy: client_sync_timeout_max
	#[serde(default = "default_timeout_max")]
	pub timeout_max: u64,

	/// Calculates room heroes in sliding and legacy client sync.
	///
	/// The Matrix specification requires this calculation, so clients may
	/// malfunction when it is disabled. Disabling is intended for custom
	/// software minimizing unused resources; otherwise the savings are
	/// negligible.
	///
	/// reloadable: yes
	/// legacy: calculate_heroes
	#[serde(default = "true_fn")]
	pub calculate_heroes: bool,
}

impl Default for Sync {
	fn default() -> Self {
		Self {
			timeout_min: default_timeout_min(),
			timeout_default: default_timeout_default(),
			timeout_max: default_timeout_max(),
			calculate_heroes: true_fn(),
		}
	}
}

fn default_timeout_min() -> u64 { 5000 }

fn default_timeout_default() -> u64 { 30000 }

fn default_timeout_max() -> u64 { 90000 }
