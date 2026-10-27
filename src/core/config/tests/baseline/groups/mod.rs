use std::collections::BTreeMap;

use itertools::Itertools;
use serde::Deserialize;
use serde_json::from_str;

use super::{assert_defaults, conditional_default, values};
use crate::config::{
	Config,
	tests::{assert_group_coverage, config_from_toml},
};

#[derive(Deserialize)]
struct Probe {
	section: String,
	field: String,
	empty: String,
	partial: String,
}

#[test]
fn grouping_sections_preserve_omitted_and_partial_defaults() {
	let probes: Vec<Probe> =
		from_str(include_str!("probes.json")).expect("independent grouping probes parse");

	assert_eq!(probes.len(), 65);
	assert_defaults(&config_from_toml("[global]\n").expect("omitted groups parse"));
	assert_group_coverage(probes.iter().map(|probe| probe.section.as_str()));

	for probe in &probes {
		let empty = config_from_toml(&probe.empty).expect("empty group parses");
		let partial = config_from_toml(&probe.partial).expect("partial group parses");

		assert_defaults(&empty);
		assert_partial(&partial, &probe.field);
	}

	let empty_groups = probes
		.iter()
		.map(|probe| format!("[{}]\n", probe.section))
		.join("");

	let config = config_from_toml(&format!("[global]\n{empty_groups}"))
		.expect("all empty groups parse together");

	assert_defaults(&config);
}

fn assert_partial(config: &Config, changed: &str) {
	let defaults: BTreeMap<String, String> =
		from_str(include_str!("../defaults.json")).expect("default baseline parses");

	let legacy: BTreeMap<String, String> =
		from_str(include_str!("../legacy.json")).expect("legacy baseline parses");

	assert_eq!(defaults.len(), 364);
	assert_eq!(legacy.len(), 364);
	for (name, actual) in values(config) {
		let conditional = conditional_default(name);
		let expected = match name {
			| name if name == changed => &legacy[name],
			| _ => conditional
				.as_ref()
				.unwrap_or_else(|| &defaults[name]),
		};

		assert_eq!(&actual, expected, "partial {changed}, field {name}");
	}
}
