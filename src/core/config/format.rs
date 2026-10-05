use std::fmt::{Display, Formatter, Result as FmtResult};

use figment::providers::Format;
use serde::de::DeserializeOwned;
use toml::de::{Error as TomlError, from_str as from_toml_str};
use url::{Position, Url};

/// TOML data format for figment's file and string providers.
///
/// figment's own provider is gated behind `toml 0.8`, a second copy of the toml
/// crate family alongside the `toml 1.x` the workspace uses. Supplying the
/// format here keeps figment's file, nesting and profile machinery while
/// parsing through the workspace crate.
pub(super) struct Toml;

/// Displays an optional relay URI with its userinfo concealed.
///
/// Unparsable or hostless values are concealed in full.
pub(super) struct UriUserinfo<'a>(pub(super) Option<&'a str>);

impl Format for Toml {
	type Error = TomlError;

	const NAME: &'static str = "TOML";

	fn from_str<T: DeserializeOwned>(text: &str) -> Result<T, Self::Error> { from_toml_str(text) }
}

impl Display for UriUserinfo<'_> {
	fn fmt(&self, out: &mut Formatter<'_>) -> FmtResult {
		let Some(value) = self.0 else {
			return out.write_str("None");
		};

		let Ok(uri) = Url::parse(value) else {
			return out.write_str("***********");
		};

		if !uri.has_host() {
			return out.write_str("***********");
		}

		if uri.username().is_empty() && uri.password().is_none() {
			return write!(out, "{:?}", Some(uri.as_str()));
		}

		write!(
			out,
			"Some(\"{}***********@{}\")",
			&uri[..Position::BeforeUsername],
			&uri[Position::BeforeHost..],
		)
	}
}
