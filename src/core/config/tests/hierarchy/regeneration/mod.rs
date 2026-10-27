use std::{fs::read_to_string, path::Path};

use ruma::UserId;

use super::{Files, isolated, options};
use crate::config::{
	Config, Sources, StorageProvider, regenerate_config, tests::baseline::assert_regenerated,
};

#[test]
fn repeated_file_occurrences_keep_the_startup_winner() {
	if !isolated("config::tests::hierarchy::regeneration::repeated_file_occurrences_keep_the_startup_winner", &[]) { return; }

	let files = Files::new();
	let first =
		files.write("first.toml", "server_name = 'repeat.example'\nrocksdb_checksums = false\n");

	let second = files.write("second.toml", "[global.rocksdb]\nchecksums = true\n");
	let output = files.0.join("repeated.toml");
	let sources = Sources {
		paths: vec![first.clone(), second, first],
		overrides: None,
	};

	let startup = Config::load_files(sources.paths.iter()).expect("ordered repeated paths load");

	assert!(
		!Config::new(&startup)
			.expect("startup config extracts")
			.rocksdb
			.checksums
	);

	regenerate_config(&sources, options(&output)).expect("repeated paths regenerate");

	let raw = Config::load_files([&output].into_iter()).expect("regenerated file loads");

	assert!(
		!Config::new(&raw)
			.expect("regenerated config extracts")
			.rocksdb
			.checksums
	);
}

#[test]
fn frozen_typed_values_survive_regeneration_and_reload() {
	if !isolated("config::tests::hierarchy::regeneration::frozen_typed_values_survive_regeneration_and_reload", &[]) { return; }

	let files = Files::new();

	for name in ["legacy.toml", "global.toml", "canonical.toml"] {
		let input = Path::new(env!("CARGO_MANIFEST_DIR"))
			.join("config/tests/baseline")
			.join(name);

		let output = files.0.join(format!("{name}.first"));
		let again = files.0.join(format!("{name}.second"));
		let sources = Sources { paths: vec![input], overrides: None };

		regenerate_config(&sources, options(&output)).expect("frozen input regenerates");

		let raw = Config::load_files([&output].into_iter()).expect("canonical output loads");
		let config = Config::new(&raw).expect("canonical output extracts");
		let rendered = read_to_string(&output).expect("canonical output read");

		assert_regenerated(&config);
		raw.find_value("database_restore_backup")
			.expect_err("noncanonical or deliberately dropped key");

		raw.find_value("force_migration")
			.expect_err("noncanonical or deliberately dropped key");

		raw.find_value("rocksdb_checksums")
			.expect_err("noncanonical or deliberately dropped key");

		let sources = Sources { paths: vec![output], overrides: None };

		regenerate_config(&sources, options(&again)).expect("canonical output regenerates");
		assert_eq!(read_to_string(again).unwrap(), rendered);
	}
}

#[test]
fn support_contacts_and_gcs_templates_keep_their_dynamic_names() {
	if !isolated("config::tests::hierarchy::regeneration::support_contacts_and_gcs_templates_keep_their_dynamic_names", &[]) { return; }

	let files = Files::new();
	let fixture = concat!(
		"[global]\nserver_name = 'dynamic.example'\n",
		"[global.well_known.support_contact.operator]\n",
		"role = 'm.role.admin'\nemail_address = 'admin@example.org'\n",
		"matrix_id = '@operator:example.org'\n",
		"pgp_key = 'https://example.org/operator.asc'\n",
		"[global.storage_provider.archive.GCS]\n",
		"bucket = 'fixture-archive'\npath = 'old/prefix'\n",
		"service_account_path = 'fixture/service-account.json'\n",
		"application_credentials_path = 'fixture/application.json'\n",
		"multipart_threshold = '23 MiB'\nmultipart_part_size = '7 MiB'\n",
		"startup_check = false\nuse_signatures = false\n",
	);

	let input = files.write("dynamic.toml", fixture);

	let output = files.0.join("dynamic-new.toml");
	let again = files.0.join("dynamic-again.toml");
	let sources = Sources { paths: vec![input], overrides: None };

	regenerate_config(&sources, options(&output)).expect("dynamic templates regenerate");

	let raw = Config::load_files([&output].into_iter()).expect("dynamic output loads");
	let config = Config::new(&raw).expect("dynamic output extracts");
	let contact = &config.well_known.support_contact["operator"];

	assert_eq!(contact.role.as_str(), "m.role.admin");
	assert_eq!(contact.email_address.as_deref(), Some("admin@example.org"));
	let matrix_id = contact.matrix_id.as_deref().map(UserId::as_str);

	assert_eq!(matrix_id, Some("@operator:example.org"));

	assert_eq!(contact.pgp_key.as_deref(), Some("https://example.org/operator.asc"));

	let StorageProvider::gcs(storage) = &config.storage_provider["archive"] else {
		panic!("GCS provider variant preserved");
	};

	assert_eq!(storage.bucket.as_deref(), Some("fixture-archive"));
	assert_eq!(storage.base_path.as_deref(), Some("old/prefix"));
	assert_eq!(storage.service_account_path.as_deref(), Some("fixture/service-account.json"));
	assert_eq!(
		storage.application_credentials_path.as_deref(),
		Some("fixture/application.json")
	);

	assert_eq!(storage.multipart_threshold.as_u64(), 23 * 1024 * 1024);
	assert_eq!(storage.multipart_part_size.as_u64(), 7 * 1024 * 1024);
	assert_eq!(storage.use_signatures, Some(false));
	assert!(!storage.startup_check);
	raw.find_value("storage_provider.archive.GCS")
		.expect_err("storage alias renders canonically");

	let rendered = read_to_string(&output).expect("dynamic output read");
	let sources = Sources { paths: vec![output], overrides: None };

	regenerate_config(&sources, options(&again)).expect("dynamic output regenerates");
	assert_eq!(read_to_string(again).unwrap(), rendered);
}
