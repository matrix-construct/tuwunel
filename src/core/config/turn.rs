//! Configures TURN relay credentials and servers.
//!
//! Shared secrets generate credentials, with static credentials available
//! as an alternative. URI, lifetime, and guest-access settings govern
//! client use.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::PathBuf;

/// Configures TURN relay credentials and servers.
///
/// Shared secrets generate credentials, with static credentials available
/// as an alternative. URI, lifetime, and guest-access settings govern
/// client use.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
// No Debug: secrets are reachable from this section.
#[derive(Clone, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.turn",
	group = "true"
)]
pub struct Turn {
	/// Allows guest users to access TURN credentials.
	///
	/// This is equivalent to Synapse's `turn_allow_guests` setting and permits
	/// guests to call `/_matrix/client/v3/voip/turnServer`.
	///
	/// reloadable: yes
	/// legacy: turn_allow_guests
	#[serde(default)]
	pub allow_guests: bool,

	/// Supplies a static TURN username when the recommended shared secret
	/// `turn.secret` is not used.
	///
	/// reloadable: yes
	/// legacy: turn_username
	#[serde(default)]
	pub username: String,

	/// Supplies a static TURN password when the recommended shared secret
	/// `turn.secret` is not used.
	///
	/// display: sensitive
	/// reloadable: yes
	/// legacy: turn_password
	#[serde(default)]
	pub password: String,

	#[expect(clippy::doc_link_with_quotes)]
	/// Lists TURN server URIs.
	///
	/// Replace "example.turn.uri" with the TURN domain, such as coturn's
	/// `realm` setting. For TURN over TLS, replace the `turn:` prefix with
	/// `turns:`.
	///
	/// reloadable: yes
	/// example: ["turn:example.turn.uri?transport=udp",
	/// "turn:example.turn.uri?transport=tcp"]
	///
	/// default: []
	/// legacy: turn_uris
	#[serde(default)]
	pub uris: Vec<String>,

	/// Supplies the TURN shared secret for HMAC-SHA1 credential generation,
	/// which is more secure than the traditional static username-and-password
	/// credentials that remain available when needed.
	///
	/// display: sensitive
	/// reloadable: yes
	/// legacy: turn_secret
	#[serde(default)]
	pub secret: Option<String>,

	/// Reads the TURN shared secret from a file.
	///
	/// This takes precedence over `turn.secret` and falls back to it when the
	/// file cannot be opened. Surrounding whitespace is trimmed so a trailing
	/// newline is not part of the secret. A present but blank file resolves to
	/// no secret without falling back.
	///
	/// reloadable: yes
	/// example: "/etc/tuwunel/.turn_secret"
	/// legacy: turn_secret_file
	pub secret_file: Option<PathBuf>,

	/// Sets the TURN lifetime in seconds.
	///
	/// reloadable: yes
	/// default: 86400
	/// legacy: turn_ttl
	#[serde(default = "default_ttl")]
	pub ttl: u64,
}

impl Default for Turn {
	fn default() -> Self {
		Self {
			allow_guests: Default::default(),
			username: Default::default(),
			password: Default::default(),
			uris: Default::default(),
			secret: Default::default(),
			secret_file: None,
			ttl: default_ttl(),
		}
	}
}

fn default_ttl() -> u64 { 60 * 60 * 24 }
