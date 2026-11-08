//! Cargo metadata captured for the running build.
//!
//! Build-time macros embed the workspace manifest and selected crate manifests.
//! This module processes that data lazily for runtime queries about project
//! features and dependencies.

#[cfg(test)]
mod tests;

use std::{collections::BTreeMap, sync::OnceLock};

use semver::VersionReq;
use serde::{Deserialize, de::IgnoredAny};
use smallstr::SmallString;
use toml::from_str;
use tuwunel_macros::cargo_manifest;

use crate::{Result, implement};

type Dependencies = BTreeMap<Name, Dependency>;
type Features = BTreeMap<String, Vec<IgnoredAny>>;
type Name = SmallString<[u8; 16]>;

/// The `[features]` table of a crate manifest.
///
/// Every other table is ignored, and a manifest without one has no features.
#[derive(Deserialize)]
struct Manifest {
	#[serde(default)]
	features: Features,
}

/// The `[workspace.dependencies]` table of the workspace manifest.
///
/// A manifest without a `[workspace]` table fails to parse.
#[derive(Deserialize)]
struct WorkspaceManifest {
	workspace: Workspace,
}

#[derive(Deserialize)]
struct Workspace {
	#[serde(default)]
	dependencies: Dependencies,
}

/// One `[workspace.dependencies]` entry, a bare version or a table.
///
/// A table without a version, such as a git or path dependency, requires any
/// version (`*`).
#[derive(Deserialize)]
#[serde(untagged)]
enum Dependency {
	Simple(VersionReq),
	Detailed {
		#[serde(default)]
		version: VersionReq,

		#[serde(default)]
		features: Vec<Name>,
	},
}

// Raw captures of the cargo manifest for each crate. This is provided by a
// proc-macro at build time since the source directory and the cargo toml's may
// not be present during execution.

#[cargo_manifest]
const WORKSPACE_MANIFEST: &'static str = ();
#[cargo_manifest(crate = "macros")]
const MACROS_MANIFEST: &'static str = ();
#[cargo_manifest(crate = "core")]
const CORE_MANIFEST: &'static str = ();
#[cargo_manifest(crate = "database")]
const DATABASE_MANIFEST: &'static str = ();
#[cargo_manifest(crate = "service")]
const SERVICE_MANIFEST: &'static str = ();
#[cargo_manifest(crate = "admin")]
const ADMIN_MANIFEST: &'static str = ();
#[cargo_manifest(crate = "router")]
const ROUTER_MANIFEST: &'static str = ();
#[cargo_manifest(crate = "main")]
const MAIN_MANIFEST: &'static str = ();

/// Processed list of features across all project crates. This is generated from
/// the data in the MANIFEST strings and contains all possible project features.
/// For *enabled* features see the info::rustc module instead.
static FEATURES: OnceLock<Vec<String>> = OnceLock::new();

/// Processed list of dependencies. This is generated from the data captured in
/// the MANIFEST.
static DEPENDENCIES: OnceLock<Dependencies> = OnceLock::new();

/// Lists dependency names declared by the workspace manifest.
///
/// Names borrow from the lazily parsed dependencies, in name order.
///
/// # Panics
///
/// Panics when the embedded workspace manifest is invalid or lacks its
/// workspace section.
pub fn dependencies_names() -> impl Iterator<Item = &'static str> {
	dependencies().map(|(name, ..)| name)
}

/// Returns dependencies declared by the embedded workspace manifest.
///
/// Each item is a name, its version requirement and the features it enables,
/// in name order. The manifest is parsed once on first access and retained for
/// the process lifetime. Package-specific dependency tables are not merged here.
///
/// # Panics
///
/// Panics when the manifest is invalid or lacks its workspace section.
pub fn dependencies() -> impl Iterator<Item = (&'static str, &'static VersionReq, &'static [Name])>
{
	DEPENDENCIES
		.get_or_init(|| {
			init_dependencies()
				.unwrap_or_else(|e| panic!("Failed to initialize dependencies: {e}"))
		})
		.iter()
		.map(|(name, dependency)| {
			(name.as_str(), dependency.version(), dependency.enabled_features())
		})
}

#[implement(Dependency)]
#[inline]
fn version(&self) -> &VersionReq {
	match self {
		| Self::Simple(version) | Self::Detailed { version, .. } => version,
	}
}

#[implement(Dependency)]
#[inline]
fn enabled_features(&self) -> &[Name] {
	match self {
		| Self::Simple(_) => &[],
		| Self::Detailed { features, .. } => features,
	}
}

/// List of all possible features for the project. For *enabled* features in
/// this build see the companion function in info::rustc.
pub fn features() -> &'static Vec<String> {
	FEATURES.get_or_init(|| {
		init_features().unwrap_or_else(|e| panic!("Failed initialize features: {e}"))
	})
}

fn init_features() -> Result<Vec<String>> {
	let mut features = Vec::new();
	append_features(&mut features, WORKSPACE_MANIFEST)?;
	append_features(&mut features, MACROS_MANIFEST)?;
	append_features(&mut features, CORE_MANIFEST)?;
	append_features(&mut features, DATABASE_MANIFEST)?;
	append_features(&mut features, SERVICE_MANIFEST)?;
	append_features(&mut features, ADMIN_MANIFEST)?;
	append_features(&mut features, ROUTER_MANIFEST)?;
	append_features(&mut features, MAIN_MANIFEST)?;
	features.sort();
	features.dedup();

	Ok(features)
}

fn append_features(features: &mut Vec<String>, manifest: &str) -> Result {
	let manifest: Manifest = from_str(manifest)?;

	features.extend(manifest.features.into_keys());

	Ok(())
}

fn init_dependencies() -> Result<Dependencies> {
	let manifest: WorkspaceManifest = from_str(WORKSPACE_MANIFEST)?;

	Ok(manifest.workspace.dependencies)
}
