//! Configures compatibility behavior across room versions.
//!
//! State-resolution backports extend existing mitigations to older
//! supported versions. The switch remains available for development and
//! recovery.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::true_fn;

/// Configures compatibility behavior across room versions.
///
/// State-resolution backports extend existing mitigations to older
/// supported versions. The switch remains available for development and
/// recovery.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.compatibility",
	group = "true"
)]
pub struct Compatibility {
	/// Backports state-reset security mitigations to all supported room
	/// versions.
	///
	/// This applies project Hydra's State Resolution 2.1 mitigations for room
	/// version 12 to prior State Resolution 2.0 versions, increasing
	/// resilience without redefining correctness and making it safe to enable
	/// for existing rooms. State-reset attacks produce rooms without one
	/// consensus, so disabling is unnecessary to match servers that disable or
	/// lack the mitigations, even when replicating their post-reset state is
	/// desired. The setting exists for developer and debug use and as a
	/// failsafe instead of being hardcoded.
	///
	/// reloadable: yes
	/// legacy: hydra_backports
	#[serde(default = "true_fn")]
	pub hydra_backports: bool,
}

impl Default for Compatibility {
	fn default() -> Self { Self { hydra_backports: true_fn() } }
}
