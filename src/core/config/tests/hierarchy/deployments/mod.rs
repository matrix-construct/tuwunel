use std::{collections::BTreeMap, env::var, iter::empty};

use figment::Profile;
use serde::Deserialize;
use serde_json::{Value, from_str};

use super::{Files, isolated};
use crate::config::Config;

#[derive(Deserialize)]
struct Fixture {
	source: String,
	environment: BTreeMap<String, String>,
	expected: BTreeMap<String, Value>,
}

#[test]
fn deployed_environment_spellings_keep_their_values() {
	let fixtures: Vec<Fixture> =
		from_str(include_str!("fixtures.json")).expect("deployment fixtures");

	assert_eq!(fixtures.len(), 5);
	if let Ok(selected) = var("CONFIG_DEPLOYMENT_CASE") {
		let fixture = fixtures
			.iter()
			.find(|fixture| fixture.source == selected)
			.expect("selected fixture");

		let raw = Config::load(empty()).expect("deployment environment loads");

		Config::new(&raw).expect("deployment values extract");
		for (path, expected) in &fixture.expected {
			let actual: Value = raw
				.extract_inner(path)
				.expect("canonical deployment setting");

			assert_eq!(&actual, expected, "{}: {path}", fixture.source);
		}

		return;
	}

	for fixture in &fixtures {
		let files = Files::new();
		let selected = files.write("selected.toml", "server_name = 'selected.example'\n");
		let selected = selected.to_str().expect("fixture path is UTF-8");

		let environment: Vec<_> = fixture
			.environment
			.iter()
			.map(|(key, value)| substitute_selector(key, value, selected))
			.chain([("CONFIG_DEPLOYMENT_CASE", fixture.source.as_str())])
			.collect();

		isolated("config::tests::hierarchy::deployments::deployed_environment_spellings_keep_their_values", &environment);
	}
}

fn substitute_selector<'a>(
	key: &'a str,
	value: &'a str,
	selected: &'a str,
) -> (&'a str, &'a str) {
	let value = if key == "TUWUNEL_CONFIG" { selected } else { value };

	(key, value)
}

#[test]
fn package_config_selectors_preserve_order_and_file_provenance() {
	if !isolated("config::tests::hierarchy::deployments::package_config_selectors_preserve_order_and_file_provenance", &[]) { return; }

	let files = Files::new();
	let inputs = [
		files.write(
			"conduit.toml",
			"server_name = 'selector.example'\nrocksdb_checksums = false\n",
		),
		files.write("conduwuit.toml", "[global.rocksdb]\nchecksums = true\n"),
		files.write("tuwunel.toml", "rocksdb_checksums = false\n"),
	];

	for count in 1..=inputs.len() {
		let environment: Vec<_> = ["CONDUIT_CONFIG", "CONDUWUIT_CONFIG", "TUWUNEL_CONFIG"]
			.into_iter()
			.zip(&inputs)
			.take(count)
			.map(|(key, path)| (key, path.to_str().expect("UTF-8 path")))
			.chain([("CONFIG_SELECTOR_EXPECTED", if count == 2 { "true" } else { "false" })])
			.collect();

		isolated("config::tests::hierarchy::deployments::selected_files_load", &environment);
	}
}

#[test]
fn selected_files_load() {
	let Ok(expected) = var("CONFIG_SELECTOR_EXPECTED") else {
		return;
	};

	let raw = Config::load(empty()).expect("selected config files load");
	let config = Config::new(&raw).expect("selected configuration extracts");
	let metadata = raw
		.find_metadata("rocksdb.checksums")
		.expect("selected file metadata");

	let native = if expected == "true" {
		"global.rocksdb.checksums"
	} else {
		"global.rocksdb_checksums"
	};

	assert_eq!(config.rocksdb.checksums.to_string(), expected);
	assert_eq!(metadata.interpolate(&Profile::Global, &["rocksdb", "checksums"]), native);
	assert_eq!(config.server_name.as_str(), "selector.example");
}
