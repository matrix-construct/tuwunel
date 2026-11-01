use figment::{Figment, providers::Format, value::Dict};

use super::{Files, config_from_toml, rejection};
use crate::config::{Config, format::Toml};

#[test]
fn three_files_preserve_unmodified_siblings_in_both_spelling_orders() {
	let files = Files::new();
	let old = files.write("old.toml", "rocksdb_checksums = false\nrocksdb_max_log_files = 9\n");

	let middle = files.write("middle.toml", "[global.rocksdb]\nlog_level = 'warn'\n");
	let new = files.write("new.toml", "[global.rocksdb]\nchecksums = true\n");

	for (paths, checksums) in [([&old, &middle, &new], true), ([&new, &middle, &old], false)] {
		let raw = Config::load_files(paths.into_iter()).expect("three sources load");
		let config = Config::new(&raw).expect("three sources extract");

		assert_eq!(config.rocksdb.checksums, checksums);
		assert_eq!(config.rocksdb.max_log_files, 9);
		assert_eq!(config.rocksdb.log_level, "warn");
	}
}

#[test]
fn separate_scalar_and_table_sources_keep_figments_combination_rule() {
	let files = Files::new();
	let scalar = files.write("scalar.toml", "rocksdb = 'not a table'\n");
	let legacy = files.write("legacy.toml", "rocksdb_checksums = false\n");
	let canonical = files.write("canonical.toml", "[global.rocksdb]\nchecksums = false\n");
	let reference_scalar =
		files.write("reference-scalar.toml", "[global]\nrocksdb = 'not a table'\n");

	for (paths, scalar_last) in [
		([&scalar, &legacy], false),
		([&legacy, &scalar], true),
		([&scalar, &canonical], false),
		([&canonical, &scalar], true),
	] {
		let reference_paths = if scalar_last {
			[&canonical, &reference_scalar]
		} else {
			[&reference_scalar, &canonical]
		};

		let expected = reference_paths
			.into_iter()
			.fold(Figment::new(), |raw, path| raw.merge(Toml::file(path).nested()));

		let actual = Config::load_files(paths.into_iter()).expect("cross-source shapes load");

		assert_eq!(actual.extract::<Dict>().unwrap(), expected.extract::<Dict>().unwrap());
		let config = Config::new(&actual);

		assert_eq!(config.is_err(), scalar_last);
		if scalar_last {
			let error = rejection(config);

			assert!(error.contains("scalar.toml"), "{error}");
		} else {
			assert!(
				!config
					.expect("higher table replaces scalar")
					.rocksdb
					.checksums
			);
		}
	}
}

#[test]
fn table_only_headerless_document_keeps_its_named_profile_boundary() {
	let files = Files::new();
	let path = files.write("table-only.toml", "rocksdb = { checksums = false }\n");
	let raw = Config::load_files([&path].into_iter()).expect("profile document loads");

	raw.find_value("rocksdb")
		.expect_err("named profile is not selected");

	assert!(
		Config::new(&raw)
			.expect("unselected profile retains defaults")
			.rocksdb
			.checksums
	);

	assert!(
		!raw.select("rocksdb")
			.extract_inner::<bool>("checksums")
			.expect("named profile remains available")
	);
}

#[test]
fn selectors_naming_a_directory_are_refused() {
	let files = Files::new();
	let error = Config::load_files([&files.0].into_iter())
		.expect_err("directory selector refused")
		.to_string();

	assert!(error.contains("are not regular files"), "{error}");
	assert!(error.contains(files.0.to_string_lossy().as_ref()), "{error}");
}

#[test]
fn rejections_without_a_file_name_the_selectors() {
	let document = "[global.rocksdb]\nchecksums = 'not a boolean'\n";
	let error = rejection(config_from_toml(document));

	assert!(error.contains("no configuration file was loaded"), "{error}");
	assert!(error.contains("--config"), "{error}");
	assert!(error.contains("TUWUNEL_CONFIG"), "{error}");

	let files = Files::new();
	let path = files.write("invalid.toml", document);
	let raw = Config::load_files([&path].into_iter()).expect("raw value accepted");
	let error = rejection(Config::new(&raw));

	assert!(!error.contains("TUWUNEL_CONFIG"), "{error}");
	assert!(
		error.starts_with("There was a problem with your configuration file: "),
		"{error}"
	);
}
