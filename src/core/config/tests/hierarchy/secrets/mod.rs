use std::fs::read_to_string;

use toml::Value;

use super::{Files, isolated, options};
use crate::config::{Config, Sources, regenerate_config, tests::config_from_toml};

#[test]
fn grouped_display_keeps_secret_values_masked() {
	let config = config_from_toml(include_str!("../../baseline/canonical.toml"))
		.expect("canonical secret fixture extracts");

	let root = config.to_string();
	let authentication = &[
		"baseline-emergency_password",
		"baseline-mas_secret",
		"baseline-oidc_registration_access_token",
	][..];

	for (rendered, secrets, visible) in [
		(config.authentication.to_string(), authentication, "access_token_ttl"),
		(
			config.registration.to_string(),
			&["baseline-registration_token", "baseline-registration_shared_secret"][..],
			"enable",
		),
		(
			config.turn.to_string(),
			&["baseline-turn_password", "baseline-turn_secret"][..],
			"ttl",
		),
		(
			config.telemetry.sentry.to_string(),
			&["https://public@example.org/1"][..],
			"filter",
		),
		(config.smtp.to_string(), &["dummy-secret"][..], "mail.example.org"),
	] {
		assert!(rendered.contains(visible), "visible section row missing");
		for secret in secrets {
			assert!(!rendered.contains(secret), "section leaked {secret}");
			assert!(!root.contains(secret), "root leaked {secret}");
		}
	}

	for label in [
		"authentication.access_token_ttl",
		"registration.enable",
		"turn.ttl",
		"rocksdb.checksums",
	] {
		assert!(root.contains(label), "root omits {label}");
	}
}

#[test]
fn secret_file_references_are_not_materialized() {
	if !isolated(
		"config::tests::hierarchy::secrets::secret_file_references_are_not_materialized",
		&[],
	) {
		return;
	}

	let files = Files::new();
	let secret = files.write("secret.txt", "secret-file-content-sentinel");
	let reference = secret.to_str().expect("UTF-8 secret reference");
	let quoted = Value::String(reference.to_owned());
	let document = format!(
		"[global]\nserver_name = 'secrets.example'\nregistration_token_file = \
		 {quoted}\nregistration_shared_secret_file = {quoted}\nturn_secret_file = {quoted}\n"
	);

	let input = files.write("input.toml", &document);
	let output = files.0.join("output.toml");
	let sources = Sources { paths: vec![input], overrides: None };

	regenerate_config(&sources, options(&output)).expect("secret references regenerate");

	let rendered = read_to_string(&output).expect("regenerated file read");
	let raw = Config::load_files([&output].into_iter()).expect("references load");

	assert!(!rendered.contains("secret-file-content-sentinel"));
	for path in ["registration.token_file", "registration.shared_secret_file", "turn.secret_file"]
	{
		let actual: String = raw
			.extract_inner(path)
			.expect("secret file reference retained");

		assert_eq!(actual, reference);
	}
}
