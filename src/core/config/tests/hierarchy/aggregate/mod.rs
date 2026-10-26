use std::{fs::read_to_string, iter::empty};

use figment::{Figment, Profile};

use super::{Files, isolated, options};
use crate::config::{Config, RegenerateOptions, Sources, regenerate_config};

#[test]
fn dictionary_environment_keeps_its_assignment_boundary() {
	if !isolated(
		"config::tests::hierarchy::aggregate::dictionary_environment_keeps_its_assignment_boundary",
		&[("TUWUNEL_ROCKSDB", "{checksums=false}")],
	) {
		return;
	}

	let raw = Config::load(empty()).expect("dictionary environment loads");

	assert!(
		!Config::new(&raw)
			.expect("dictionary environment extracts")
			.rocksdb
			.checksums
	);

	assert_native(&raw, "rocksdb.checksums", &["rocksdb", "checksums"], "TUWUNEL_ROCKSDB");
	assert_annotation("TUWUNEL_ROCKSDB", "TUWUNEL_ROCKSDB__CHECKSUMS");
}

#[test]
fn array_environment_keeps_its_assignment_boundary() {
	if !isolated(
		"config::tests::hierarchy::aggregate::array_environment_keeps_its_assignment_boundary",
		&[("TUWUNEL_IDENTITY_PROVIDER", "[{brand=generic,client_id=demo}]")],
	) {
		return;
	}

	let raw = Config::load(empty()).expect("array environment loads");
	let config = Config::new(&raw).expect("array environment extracts");

	assert_eq!(config.identity_provider["0"].client_id, "demo");
	assert_native(
		&raw,
		"identity_provider",
		&["identity_provider", "0", "client_id"],
		"TUWUNEL_IDENTITY_PROVIDER",
	);

	assert_annotation("TUWUNEL_IDENTITY_PROVIDER", "TUWUNEL_IDENTITY_PROVIDER__CLIENT_ID");
}

#[test]
fn malformed_dictionary_leaf_names_the_supplied_variable() {
	if !isolated(
		"config::tests::hierarchy::aggregate::malformed_dictionary_leaf_names_the_supplied_variable",
		&[("TUWUNEL_ROCKSDB", "{checksums=secret-sentinel}")],
	) {
		return;
	}

	assert_error("TUWUNEL_ROCKSDB", "TUWUNEL_ROCKSDB__CHECKSUMS");
}

#[test]
fn malformed_array_leaf_names_the_supplied_variable() {
	if !isolated(
		"config::tests::hierarchy::aggregate::malformed_array_leaf_names_the_supplied_variable",
		&[("TUWUNEL_IDENTITY_PROVIDER", "[{brand=generic,client_id={bad=secret-sentinel}}]")],
	) {
		return;
	}

	assert_error("TUWUNEL_IDENTITY_PROVIDER", "TUWUNEL_IDENTITY_PROVIDER__CLIENT_ID");
}

fn assert_native(raw: &Figment, path: &str, keys: &[&str], native: &str) {
	let metadata = raw
		.find_metadata(path)
		.expect("environment metadata retained");

	assert_eq!(metadata.interpolate(&Profile::Global, keys), native);
}

fn assert_annotation(native: &str, invented: &str) {
	let files = Files::new();
	let input = files.write("input.toml", "server_name = 'aggregate.example'\n");
	let sources = Sources { paths: vec![input], overrides: None };

	for include_env in [false, true] {
		let output = files.0.join(format!("{include_env}.toml"));
		let selected = RegenerateOptions { include_env, ..options(&output) };

		regenerate_config(&sources, selected).expect("aggregate environment regenerates");

		let rendered = read_to_string(&output).expect("regenerated output read");

		assert!(rendered.contains(native), "missing actual assignment {native}");
		if include_env {
			assert!(rendered.contains(&format!("# materialized from {native}.")));
		}

		assert!(!rendered.contains(invented), "invented assignment {invented}");
	}
}

fn assert_error(native: &str, invented: &str) {
	let raw = Config::load(empty()).expect("aggregate value loads before typed extraction");
	let structured = match native {
		| "TUWUNEL_ROCKSDB" => raw
			.find_value("rocksdb")
			.is_ok_and(|value| value.as_dict().is_some()),
		| "TUWUNEL_IDENTITY_PROVIDER" => raw
			.find_value("identity_provider")
			.is_ok_and(|value| value.as_array().is_some()),
		| _ => false,
	};

	assert!(structured, "malformed fixture is an aggregate with a bad leaf");
	let error = Config::new(&raw)
		.err()
		.expect("malformed leaf rejected")
		.to_string();

	assert!(error.contains(native), "{error}");
	assert!(!error.contains(invented), "{error}");
	assert!(!error.contains("secret-sentinel"), "{error}");
}
