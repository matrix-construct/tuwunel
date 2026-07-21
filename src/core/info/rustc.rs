//! Rust compiler metadata captured for the running build.
//!
//! Participating project crates contribute their compiler flags through
//! build-time macros and static initialization, allowing project-wide compiler
//! information to be queried here.

use std::{
	collections::BTreeMap,
	mem::replace,
	sync::{Mutex, OnceLock},
};

// Capture rustc version during compilation.
tuwunel_macros::rustc_version! {}

/// Compiler flags captured for participating project crates.
///
/// Crate-local `rustc_flags_capture` macros populate this map during static
/// initialization. It is public only for that registration path and must not be
/// modified elsewhere.
pub static FLAGS: Mutex<BTreeMap<&str, &[&str]>> = Mutex::new(BTreeMap::new());

/// Processed list of enabled features across participating project crates. This
/// is generated from the data in FLAGS.
static FEATURES: OnceLock<Vec<&'static str>> = OnceLock::new();

/// List of features enabled for the project.
pub fn features() -> &'static Vec<&'static str> { FEATURES.get_or_init(init_features) }

/// Version of the rustc compiler used during build.
#[inline]
#[must_use]
pub fn version() -> Option<&'static str> {
	RUSTC_VERSION
		.len()
		.gt(&0)
		.then_some(RUSTC_VERSION)
}

fn init_features() -> Vec<&'static str> {
	let mut features = Vec::new();
	FLAGS
		.lock()
		.expect("locked")
		.iter()
		.for_each(|(_, flags)| append_features(&mut features, flags));

	features.sort_unstable();
	features.dedup();
	features
}

fn append_features(features: &mut Vec<&'static str>, flags: &[&'static str]) {
	let mut next_is_cfg = false;
	for flag in flags {
		let is_cfg = *flag == "--cfg";
		let is_feature = flag.starts_with("feature=");
		if replace(&mut next_is_cfg, is_cfg)
			&& is_feature
			&& let Some(feature) = flag
				.split_once('=')
				.map(|(_, feature)| feature.trim_matches('"'))
		{
			features.push(feature);
		}
	}
}
