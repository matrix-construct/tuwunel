//! Configures encrypted rooms and device-key distribution.
//!
//! Room defaults select when encryption is enabled automatically. Key
//! limits and update-recipient policy govern device encryption data.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::true_fn;

/// Configures encrypted rooms and device-key distribution.
///
/// Room defaults select when encryption is enabled automatically. Key
/// limits and update-recipient policy govern device encryption data.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.encryption",
	group = "true"
)]
pub struct Encryption {
	/// Allows encrypted rooms and events.
	///
	/// reloadable: yes
	/// legacy: allow_encryption
	#[serde(default = "true_fn")]
	pub allow: bool,

	/// Selects which locally created rooms use end-to-end encryption by
	/// default.
	///
	/// "all" encrypts every created room, while "invite" encrypts rooms using
	/// the `private_chat` or `trusted_private_chat` preset. The explicit "none"
	/// value and all other values have no effect.
	///
	/// reloadable: yes
	/// default: "none"
	/// legacy: encryption_enabled_by_default_for_room_type
	#[serde(default)]
	pub enabled_by_default_for_room_type: Option<String>,

	/// Limits one-time keys per device across all algorithms.
	///
	/// The default allows 256 keys; there is no known reason for an
	/// administrator to change it.
	///
	/// reloadable: yes
	/// default: 256
	/// legacy: one_time_key_limit
	#[serde(default = "default_one_time_key_limit")]
	pub one_time_key_limit: usize,

	/// Excludes unencrypted rooms when selecting common-room recipients of
	/// device-list updates.
	///
	/// Enabling this can improve performance on very large servers but may
	/// violate the specification and client expectations.
	///
	/// reloadable: yes
	/// legacy: device_key_update_encrypted_rooms_only
	#[serde(default)]
	pub device_key_update_encrypted_rooms_only: bool,
}

impl Default for Encryption {
	fn default() -> Self {
		Self {
			allow: true_fn(),
			enabled_by_default_for_room_type: Default::default(),
			one_time_key_limit: default_one_time_key_limit(),
			device_key_update_encrypted_rooms_only: Default::default(),
		}
	}
}

fn default_one_time_key_limit() -> usize { 256 }
