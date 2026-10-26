//! Qualifies unknown keys at every migrated grouping depth.
//!
//! Independent literal cases cover diagnostics, root exemptions, and preservation
//! through repeated regeneration as additional groups become available.

use std::fs::read_to_string;

use serde::Deserialize;
use serde_json::from_str;

use super::{Files, options};
use crate::config::{
	Config, Sources,
	regenerate::schema,
	regenerate_config,
	tests::{check_with_captured_logs, config_from_toml},
};

#[derive(Deserialize)]
struct Case {
	section: String,
	unknown_input: String,
	unknown_path: String,
	runtime_path: String,
}

#[test]
fn unknown_keys_obey_warning_and_strict_modes() {
	cases().for_each(|case| {
		for strict in [false, true] {
			check_unknown(&case, strict);
		}
	});
}

fn cases() -> impl Iterator<Item = Case> {
	let cases: Vec<Case> = from_str(include_str!("../../baseline/groups/probes.json"))
		.expect("independent group cases parse");

	assert_eq!(cases.len(), 65);
	schema()
		.iter()
		.filter(|section| section.group)
		.for_each(|section| {
			assert!(
				cases
					.iter()
					.any(|case| case.section == section.section)
			);
		});

	cases.into_iter().filter(|case| {
		schema()
			.iter()
			.any(|section| section.group && section.section == case.section)
	})
}

fn check_unknown(case: &Case, strict: bool) {
	let document = format!(
		"[global]\nerror_on_unknown_config_opts = {strict}\nruntime_root_only = true\n{}",
		case.unknown_input
	);

	let config = config_from_toml(&document).expect("unknown group keys retained");
	let (result, logs) = check_with_captured_logs(&config);

	assert!(logs.contains(&case.unknown_path), "{logs}");
	assert!(logs.contains(&case.runtime_path), "{logs}");
	assert!(!logs.contains("runtime_root_only"), "{logs}");
	match result {
		| Ok(()) => assert!(!strict),
		| Err(error) => {
			let message = error.to_string();

			assert!(strict, "{message}");
			assert!(message.contains(&case.unknown_path), "{message}");
			assert!(message.contains(&case.runtime_path), "{message}");
			assert!(!message.contains("runtime_root_only"), "{message}");
		},
	}
}

#[test]
fn root_runtime_keys_remain_exempt_in_both_modes() {
	for strict in [false, true] {
		let document = format!(
			"[global]\nerror_on_unknown_config_opts = {strict}\nruntime_root_only = true\n"
		);

		let config = config_from_toml(&document).expect("root runtime key retained");
		let (result, logs) = check_with_captured_logs(&config);

		assert!(result.is_ok(), "{result:?}");
		assert!(!logs.contains("runtime_root_only"), "{logs}");
	}
}

#[test]
fn unknown_residue_survives_idempotent_regeneration() {
	cases().for_each(|case| {
		let files = Files::new();
		let document = format!(
			"[global]\nserver_name = 'groups.example'\nruntime_root_only = true\n{}",
			case.unknown_input
		);

		let input = files.write("input.toml", &document);
		let output = files.0.join("canonical.toml");
		let second = files.0.join("again.toml");
		let sources = Sources { paths: vec![input], overrides: None };

		regenerate_config(&sources, options(&output)).expect("group residue regenerated");

		let rendered = read_to_string(&output).expect("output read");
		let raw = Config::load_files([&output].into_iter()).expect("output loaded");
		let residue: Vec<String> = raw
			.extract_inner(&case.unknown_path)
			.expect("array residue");

		let nested_runtime: bool = raw
			.extract_inner(&case.runtime_path)
			.expect("nested residue");

		let root_runtime: bool = raw
			.extract_inner("runtime_root_only")
			.expect("root residue");

		assert_eq!(residue, ["retained"]);
		assert!(nested_runtime);
		assert!(root_runtime);

		let sources = Sources { paths: vec![output], overrides: None };

		regenerate_config(&sources, options(&second)).expect("second group regeneration");
		assert_eq!(read_to_string(second).expect("second output read"), rendered);
	});
}
