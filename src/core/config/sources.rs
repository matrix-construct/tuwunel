#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use super::{Config, Figment};
use crate::{Result, implement};

/// Applies the entry point's own contribution to a freshly loaded
/// configuration.
pub type Overrides = dyn Fn(Figment) -> Result<Figment> + Send + Sync;

/// Retains the inputs used to assemble a server configuration.
///
/// Reloads reuse these paths and the optional override to reproduce startup
/// behavior. Environment providers are added by the configuration loader
/// itself.
#[derive(Default)]
pub struct Sources {
	/// Lists configuration files in merge order.
	///
	/// Later paths overlay values from earlier paths. Reloads begin with this
	/// same ordered sequence before adding any explicit extra paths.
	pub paths: Vec<PathBuf>,

	/// Applies an optional transform after the standard providers are merged.
	///
	/// The callback can add synthetic overrides after standard loading. Reloads
	/// retain it so those values are not silently discarded.
	pub overrides: Option<Box<Overrides>>,
}

/// Builds a raw configuration from these sources, with `extra` paths layered
/// after them.
#[implement(Sources)]
pub fn load<'a, I>(&'a self, extra: I) -> Result<Figment>
where
	I: Iterator<Item = &'a Path>,
{
	let paths = self
		.paths
		.iter()
		.map(PathBuf::as_path)
		.chain(extra);

	Config::load(paths).and_then(|raw| self.apply(raw))
}

#[implement(Sources)]
fn apply(&self, raw: Figment) -> Result<Figment> {
	match &self.overrides {
		| None => Ok(raw),
		| Some(overrides) => overrides(raw),
	}
}
