use std::fs::read_to_string;

use figment::Profile;

use super::{Files, isolated, options, rejection};
use crate::config::{Config, Sources, regenerate_config};

#[test]
fn logging_preserves_provider_order_and_native_metadata() {
	let files = Files::new();
	let canonical = files.write("canonical.toml", "[global.logging]\ncolors = true\n");

	for (document, native) in [
		("[global]\nlog_colours = false\n", "global.log_colours"),
		("[global.logging]\nlog_colours = false\n", "global.logging.log_colours"),
	] {
		let alias = files.write("alias.toml", document);

		for (paths, expected, native) in [
			([&alias, &canonical], true, "global.logging.colors"),
			([&canonical, &alias], false, native),
		] {
			let raw = Config::load_files(paths.into_iter()).expect("ordered aliases load");
			let config = Config::new(&raw).expect("actual logging schema extracts");
			let value = raw
				.find_value("logging.colors")
				.expect("canonical value");

			let metadata = raw
				.get_metadata(value.tag())
				.expect("winning source metadata");

			let actual = metadata.interpolate(&Profile::Global, &["logging", "colors"]);

			assert_eq!(config.logging.colors, expected);
			assert_eq!(actual, native);
			assert!(!raw.contains("log_colours"));
			assert!(!raw.contains("logging.log_colours"));
		}
	}
}

#[test]
fn logging_conflicts_reject_both_textual_orders() {
	let files = Files::new();

	for legacy in [
		"log_colours = true\n",
		"log_colours = false\n",
		"logging.log_colours = true\n",
		"logging.log_colours = false\n",
	] {
		let canonical = "logging.colors = true\n";

		for (first, last) in [(legacy, canonical), (canonical, legacy)] {
			let document = format!("[global]\n{first}{last}");
			let path = files.write("conflict.toml", &document);
			let error = Config::load_files([&path].into_iter())
				.expect_err("same-source duplicate rejected")
				.to_string();

			assert!(error.contains("conflict.toml"), "{error}");
			assert!(error.contains("log_colours"), "{error}");
			assert!(error.contains("logging.colors"), "{error}");
		}
	}
}

#[test]
fn logging_type_errors_report_the_winning_native_key() {
	let files = Files::new();
	let canonical = files.write("canonical.toml", "[global.logging]\ncolors = true\n");

	for native in ["log_colours", "logging.log_colours"] {
		let document = format!("[global]\n{native} = 'yes'\n");
		let alias = files.write("invalid-alias.toml", &document);
		let raw = Config::load_files([&canonical, &alias].into_iter()).expect("raw alias loads");
		let error = rejection(Config::new(&raw));

		assert!(error.contains(native), "{error}");
		assert!(error.contains("invalid-alias.toml"), "{error}");
	}
}

#[test]
fn logging_regenerates_only_the_canonical_spelling() {
	if !isolated(
		"config::tests::hierarchy::aliases::logging_regenerates_only_the_canonical_spelling",
		&[],
	) {
		return;
	}

	let files = Files::new();

	for native in ["log_colours", "logging.log_colours"] {
		let document = format!("[global]\nserver_name = 'alias.example'\n{native} = false\n");
		let input = files.write("alias.toml", &document);
		let output = files.0.join(format!("{native}-canonical.toml"));
		let sources = Sources { paths: vec![input], overrides: None };

		regenerate_config(&sources, options(&output)).expect("actual alias regenerates");

		let rendered = read_to_string(&output).expect("canonical output readable");
		let raw = Config::load_files([&output].into_iter()).expect("canonical output loads");
		let config = Config::new(&raw).expect("canonical output extracts");

		assert!(!config.logging.colors);
		assert!(rendered.contains("[global.logging]"));
		assert!(rendered.contains("colors = false"));
		assert!(!rendered.contains("log_colours"));
		assert!(!rendered.contains("log_colors"));
	}
}
