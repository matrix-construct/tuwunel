//! Cargo metadata captured for the running build.
//!
//! Build-time macros embed the workspace manifest and selected crate manifests.
//! This module processes that data lazily for runtime queries about project
//! features and dependencies.

use std::sync::OnceLock;

use cargo_toml::{DepsSet, Manifest};
use tuwunel_macros::cargo_manifest;

use crate::Result;

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
static DEPENDENCIES: OnceLock<DepsSet> = OnceLock::new();

#[must_use]
pub fn dependencies_names() -> Vec<&'static str> {
	dependencies()
		.keys()
		.map(String::as_str)
		.collect()
}

pub fn dependencies() -> &'static DepsSet {
	DEPENDENCIES.get_or_init(|| {
		init_dependencies().unwrap_or_else(|e| panic!("Failed to initialize dependencies: {e}"))
	})
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
	let manifest = Manifest::from_str(manifest)?;
	features.extend(manifest.features.keys().cloned());

	Ok(())
}

fn init_dependencies() -> Result<DepsSet> {
	let manifest = Manifest::from_str(WORKSPACE_MANIFEST)?;
	let deps_set = manifest
		.workspace
		.as_ref()
		.expect("manifest has workspace section")
		.dependencies
		.clone();

	Ok(deps_set)
}
