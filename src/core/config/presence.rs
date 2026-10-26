//! Configures local and federated presence.
//!
//! Incoming and outgoing updates have separate controls. Idle and offline
//! timers determine how inactivity changes presence.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::true_fn;

/// Configures local and federated presence.
///
/// Incoming and outgoing updates have separate controls. Idle and offline
/// timers determine how inactivity changes presence.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.presence",
	group = "true"
)]
pub struct Presence {
	/// Allows local presence updates and requests.
	///
	/// This must be enabled when using `presence.allow_outgoing`.
	///
	/// reloadable: yes
	/// legacy: allow_local_presence
	#[serde(default = "true_fn")]
	pub allow_local: bool,

	/// Allows incoming federated presence updates and requests.
	///
	/// This receives updates but sends none unless `presence.allow_outgoing`
	/// is enabled.
	///
	/// reloadable: yes
	/// legacy: allow_incoming_presence
	#[serde(default = "true_fn")]
	pub allow_incoming: bool,

	/// Allows outgoing federated presence updates and requests.
	///
	/// This sends updates but receives none unless `presence.allow_incoming`
	/// is enabled, and it requires `presence.allow_local`.
	///
	/// reloadable: yes
	/// legacy: allow_outgoing_presence
	#[serde(default = "true_fn")]
	pub allow_outgoing: bool,

	/// Sets the seconds without presence updates before a user becomes idle,
	/// defaulting to five minutes.
	///
	/// default: 300
	/// legacy: presence_idle_timeout_s
	#[serde(default = "default_idle_timeout_s")]
	pub idle_timeout_s: u64,

	/// Sets the seconds without presence updates before a user becomes
	/// offline, defaulting to thirty minutes.
	///
	/// default: 1800
	/// legacy: presence_offline_timeout_s
	#[serde(default = "default_offline_timeout_s")]
	pub offline_timeout_s: u64,

	/// Enables the presence idle timer for remote users.
	///
	/// Disabling it can reduce resource use on servers participating in many
	/// large rooms or with limited resources. Some remote users may then show
	/// incorrect presence states, such as remaining online indefinitely.
	///
	/// legacy: presence_timeout_remote_users
	#[serde(default = "true_fn")]
	pub timeout_remote_users: bool,
}

impl Default for Presence {
	fn default() -> Self {
		Self {
			allow_local: true_fn(),
			allow_incoming: true_fn(),
			allow_outgoing: true_fn(),
			idle_timeout_s: default_idle_timeout_s(),
			offline_timeout_s: default_offline_timeout_s(),
			timeout_remote_users: true_fn(),
		}
	}
}

fn default_idle_timeout_s() -> u64 { 5 * 60 }

fn default_offline_timeout_s() -> u64 { 30 * 60 }
