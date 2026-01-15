//! Information about the build related to rustc. This is a frontend interface
//! informed by proc-macros at build time. Since the project is split into
//! several crates, lower-level information is supplied from each crate during
//! static initialization.

use std::{
	collections::BTreeMap,
	mem::replace,
	sync::{Mutex, OnceLock},
};

// Capture rustc version during compilation.
tuwunel_macros::rustc_version! {}

/// Raw capture of rustc flags used to build each crate in the project. Informed
/// by rustc_flags_capture macro (one in each crate's mod.rs). This is
/// done during static initialization which is why it's mutex-protected and pub.
/// Should not be written to by anything other than our macro.
pub static FLAGS: Mutex<BTreeMap<&str, &[&str]>> = Mutex::new(BTreeMap::new());

/// Processed list of enabled features across all project crates. This is
/// generated from the data in FLAGS.
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
