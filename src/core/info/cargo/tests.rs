use itertools::assert_equal;
use toml::from_str;

use super::{Manifest, WorkspaceManifest, dependencies, dependencies_names, features};

#[test]
fn dependencies_render_requirement_and_features() {
	let manifest: WorkspaceManifest = from_str(concat!(
		"[workspace.dependencies]\n",
		"plain = \"1.0\"\n",
		"detailed = { version = \"0.8.3\", features = [\"a\", \"b\"] }\n",
		"git = { git = \"https://example.org/x.git\", rev = \"0\", features = [\"c\"] }\n",
		"bare = { path = \"x\" }\n",
		"\"\\u0065scaped\" = { version = \"1\", features = [\"s\\u0065rde\"] }\n",
	))
	.expect("fixture parses");

	let rows = manifest
		.workspace
		.dependencies
		.iter()
		.map(|(name, dependency)| {
			let features = dependency.enabled_features().join(" ");

			format!("{name} {} {features}", dependency.version())
		});

	let expected =
		["bare * ", "detailed ^0.8.3 a b", "escaped ^1 serde", "git * c", "plain ^1.0 "];

	assert_equal(rows, expected);
}

#[test]
fn manifest_features_are_table_keys() {
	let manifest: Manifest = from_str(concat!("[features]\n", "default = [\"x\"]\n", "x = []\n"))
		.expect("features parse");

	assert_equal(manifest.features.keys(), ["default", "x"]);

	let manifest: Manifest =
		from_str("[workspace]\n").expect("a manifest without features parses");

	assert!(manifest.features.is_empty());
}

#[test]
fn embedded_manifests_parse() {
	let declared = |name: &str| features().iter().any(|feature| feature == name);

	assert!(features().is_sorted_by(|a, b| a < b), "features are sorted and deduplicated");
	assert!(declared("default"));
	assert!(declared("release_max_log_level"));
	assert!(dependencies().any(|(name, ..)| name == "tokio"));
	assert_equal(dependencies_names(), dependencies().map(|(name, ..)| name));
}
