use std::panic::catch_unwind;

use figment::{Error, Figment, Profile, Provider, providers::Format, value::Dict};

use super::{
	FieldClass, FieldSpec, SectionSpec, environment_assignment, normalize, normalize_values,
	normalize_with, validate_registry,
};
use crate::config::{format::Toml, regenerate::render_fixture};

static CHECKSUMS: FieldSpec = FieldSpec {
	name: "checksums",
	path: "rocksdb.checksums",
	legacy: &["rocksdb_checksums"],
	aliases: &[],
	example: "",
	class: FieldClass::Documented,
};

static COLORS: FieldSpec = FieldSpec {
	name: "colors",
	path: "logging.colors",
	legacy: &["log_colors", "log_colours", "logging.log_colours"],
	aliases: &["log_colours"],
	example: "",
	class: FieldClass::Documented,
};

#[test]
fn rewrites_explicit_legacy_values_without_defaults() {
	let actual = normalized("rocksdb_checksums = false\nlog_colours = true")
		.expect("legacy input accepted");

	let expected = values("[rocksdb]\nchecksums = false\n[logging]\ncolors = true");

	assert_eq!(actual, expected);
	assert!(
		normalized("")
			.expect("empty provider accepted")
			.is_empty()
	);
}

fn normalized(input: &str) -> Result<Dict, Error> {
	let aliases = [&CHECKSUMS, &COLORS]
		.into_iter()
		.flat_map(|field| {
			field
				.legacy
				.iter()
				.map(move |from| (*from, field))
		});

	normalize_values(values(input), aliases)
}

fn values(input: &str) -> Dict {
	Toml::string(input)
		.data()
		.expect("valid fixture")
		.remove(&Profile::Default)
		.unwrap_or_default()
}

#[test]
fn rejects_duplicate_spellings_and_scalar_ancestors() {
	for input in [
		"rocksdb_checksums = false\n[rocksdb]\nchecksums = false",
		"rocksdb_checksums = true\n[rocksdb]\nchecksums = false",
		"log_colors = true\nlog_colours = false",
		"[logging]\ncolors = false\nlog_colours = true",
		"rocksdb_checksums = false\nrocksdb = 'secret sentinel'",
	] {
		let error = normalized(input)
			.expect_err("ambiguous input rejected")
			.to_string();

		assert!(!error.contains("secret sentinel"));
	}
}

#[test]
fn leaves_terminal_maps_and_unknown_keys_intact() {
	let input = "__aliased = 'untrusted'\n[other]\nlog_colours = \
	             'opaque'\n[opaque]\nrocksdb_checksums = false";

	assert_eq!(normalized(input).expect("residue retained"), values(input));
}

#[test]
fn rejects_ancestor_related_legacy_paths_in_either_order() {
	static PARENT: SectionSpec = SectionSpec {
		section: "new",
		aliases: &[],
		example: "",
		fields: &[FieldSpec { legacy: &["old"], ..CHECKSUMS }],
		group: false,
	};

	static CHILD: SectionSpec = SectionSpec {
		section: "other",
		aliases: &[],
		example: "",
		fields: &[FieldSpec { legacy: &["old.child"], ..COLORS }],
		group: false,
	};

	for sections in [[&PARENT, &CHILD], [&CHILD, &PARENT]] {
		assert!(catch_unwind(|| validate_registry(&sections)).is_err());
	}
}

#[test]
fn rejects_terminal_alias_and_reserved_schema_collisions() {
	static TERMINAL: SectionSpec = SectionSpec {
		section: "global",
		aliases: &[],
		example: "",
		fields: &[
			FieldSpec {
				name: "opaque",
				path: "opaque",
				legacy: &[],
				..CHECKSUMS
			},
			FieldSpec { legacy: &["opaque.old"], ..CHECKSUMS },
		],
		group: false,
	};

	static ALIAS: SectionSpec = SectionSpec {
		fields: &[
			FieldSpec {
				path: "log_colors",
				legacy: &[],
				aliases: &["log_colours"],
				..COLORS
			},
			FieldSpec { legacy: &["log_colours"], ..CHECKSUMS },
		],
		..TERMINAL
	};

	static RESERVED_FIELD: SectionSpec = SectionSpec {
		fields: &[FieldSpec {
			path: "config.control",
			legacy: &[],
			..CHECKSUMS
		}],
		..TERMINAL
	};

	static RESERVED_SECTION: SectionSpec = SectionSpec {
		section: "global.config",
		fields: &[],
		..TERMINAL
	};

	for section in [&TERMINAL, &ALIAS, &RESERVED_FIELD, &RESERVED_SECTION] {
		assert!(catch_unwind(|| validate_registry(&[section])).is_err());
	}
}

#[test]
fn preserves_tagged_metadata_and_consumes_snapshot_once() {
	let original = Figment::from(Toml::string("rocksdb_checksums = false"));
	let value = original
		.find_value("rocksdb_checksums")
		.expect("tagged input");

	let metadata = original
		.get_metadata(value.tag())
		.expect("input metadata");

	let expected = metadata.interpolate(&Profile::Default, &["rocksdb_checksums"]);
	let provider = normalize(original).expect("normalized tagged provider");
	let actual = Figment::from(&provider);
	let value = actual
		.find_value("rocksdb.checksums")
		.expect("canonical output");

	let metadata = actual
		.get_metadata(value.tag())
		.expect("output metadata");

	assert_eq!(metadata.interpolate(&Profile::Default, &["rocksdb", "checksums"]), expected);
	provider
		.data()
		.expect_err("snapshot cannot be consumed twice");
}

#[test]
fn section_local_aliases_keep_source_precedence_and_provenance() {
	static LOGGING: SectionSpec = SectionSpec {
		section: "global.logging",
		aliases: &[],
		example: "[global.logging]\n#colors = true\n",
		fields: &[COLORS],
		group: true,
	};

	static ROOT: SectionSpec = SectionSpec {
		section: "global",
		aliases: &[],
		example: "",
		fields: &[FieldSpec {
			name: "logging",
			path: "logging",
			legacy: &[],
			class: FieldClass::Structural,
			..CHECKSUMS
		}],
		group: false,
	};

	validate_registry(&[&ROOT, &LOGGING]);
	let alias = "[logging]\nlog_colours = false";
	let canonical = "[logging]\ncolors = true";

	for (first, last, expected, key) in [
		(alias, canonical, true, "logging.colors"),
		(canonical, alias, false, "logging.log_colours"),
	] {
		let normalize = |input| normalize_with(Toml::string(input), &[&LOGGING]);
		let actual = Figment::from(normalize(first).expect("first source"))
			.merge(normalize(last).expect("last source"));

		let value = actual
			.find_value("logging.colors")
			.expect("winning value");

		let metadata = actual
			.get_metadata(value.tag())
			.expect("winning metadata");

		let native = metadata.interpolate(&Profile::Default, &["logging", "colors"]);

		assert_eq!(value.to_bool(), Some(expected));
		assert_eq!(native, format!("default.{key}"));
		assert!(!actual.contains("logging.log_colours"));

		let values = actual
			.extract::<Dict>()
			.expect("canonical dictionary");

		let rendered = render_fixture(&LOGGING, &values).expect("canonical rendering");
		let reloaded = Figment::from(Toml::string(&rendered).nested());

		let reloaded = reloaded
			.extract_inner::<bool>("logging.colors")
			.unwrap();

		assert_eq!(reloaded, expected);

		assert!(!rendered.contains("log_colours"));
	}
}

#[test]
fn aggregate_and_leaf_assignments_keep_native_merge_order() {
	let aggregate = ("TUWUNEL_ROCKSDB", "{checksums=false}");
	let leaf = ("TUWUNEL_ROCKSDB__CHECKSUMS", "true");

	for (first, last, expected) in [(aggregate, leaf, true), (leaf, aggregate, false)] {
		let assignment = |(name, value): (&str, &str)| {
			environment_assignment("TUWUNEL_", name, value).unwrap()
		};

		let original = Figment::from(assignment(first)).merge(assignment(last));
		let actual = Figment::from(normalize(original).expect("normalized assignments"));
		let value = actual.find_value("rocksdb.checksums").unwrap();
		let metadata = actual.get_metadata(value.tag()).unwrap();

		assert_eq!(value.to_bool(), Some(expected));
		assert_eq!(metadata.interpolate(&Profile::Global, &["rocksdb", "checksums"]), last.0);
		assert_eq!(actual.profile(), &Profile::Default);
	}
}
