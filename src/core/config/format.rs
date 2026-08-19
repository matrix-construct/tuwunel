use figment::providers::Format;
use serde::de::DeserializeOwned;
use toml::de::{Error as TomlError, from_str as from_toml_str};

/// TOML data format for figment's file and string providers.
///
/// figment's own provider is gated behind `toml 0.8`, a second copy of the toml
/// crate family alongside the `toml 1.x` the workspace uses. Supplying the
/// format here keeps figment's file, nesting and profile machinery while
/// parsing through the workspace crate.
pub(super) struct Toml;

impl Format for Toml {
	type Error = TomlError;

	const NAME: &'static str = "TOML";

	fn from_str<T: DeserializeOwned>(text: &str) -> Result<T, Self::Error> { from_toml_str(text) }
}
