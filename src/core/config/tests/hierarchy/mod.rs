mod aggregate;
mod aliases;
mod compatibility;
mod groups;
mod loading;
mod regeneration;

use std::{
	env::{current_exe, temp_dir, var, vars_os},
	fs::{create_dir_all, read_to_string, remove_dir_all, write},
	iter::empty,
	path::{Path, PathBuf},
	process::{Command, id},
	sync::atomic::{AtomicU64, Ordering},
};

use figment::Profile;
use tuwunel_macros::implement;

use super::{baseline::assert_defaults, check_with_captured_logs, config_from_toml};
use crate::config::{Config, ENV_PREFIXES, RegenerateOptions, Sources, regenerate_config};

static NEXT: AtomicU64 = AtomicU64::new(0);
const CHILD: &str = "CONFIG_HIERARCHY_CHILD";
const EXPECTED: &str = "CONFIG_HIERARCHY_EXPECTED";
const NATIVE: &str = "CONFIG_HIERARCHY_NATIVE";

struct Files(PathBuf);

#[test]
fn rocksdb_absent_empty_and_partial_preserve_old_defaults() {
	for input in ["[global]\n", "[global.rocksdb]\n"] {
		assert_defaults(&config_from_toml(input).expect("default section accepted"));
	}

	let partial = config_from_toml("[global.rocksdb]\nchecksums = false\n")
		.expect("partial section accepted");

	assert!(!partial.rocksdb.checksums);
	assert_defaults(&with_default_checksums(partial));
}

fn with_default_checksums(mut config: Config) -> Config {
	config.rocksdb.checksums = true;
	config
}

#[test]
fn legacy_and_canonical_files_obey_order_and_preserve_siblings() {
	let files = Files::new();
	let legacy =
		files.write("old.toml", "rocksdb_checksums = false\nrocksdb_max_log_files = 9\n");

	let canonical = files.write("new.toml", "[global.rocksdb]\nchecksums = true\n");

	for (paths, expected) in [([&legacy, &canonical], true), ([&canonical, &legacy], false)] {
		let raw = Config::load_files(paths.into_iter()).expect("ordered sources accepted");
		let config = Config::new(&raw).expect("typed configuration");

		assert_eq!(config.rocksdb.checksums, expected);
		assert_eq!(config.rocksdb.max_log_files, 9);
	}
}

#[implement(Files)]
fn new() -> Self {
	let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
	let path = temp_dir().join(format!("tuwunel-config-hierarchy-{}-{sequence}", id()));

	create_dir_all(&path).expect("fixture directory created");
	Self(path)
}

#[implement(Files)]
fn write(&self, name: &str, contents: &str) -> PathBuf {
	let path = self.0.join(name);

	write(&path, contents).expect("fixture written");
	path
}

#[test]
fn canonical_toml_spellings_are_equivalent() {
	for input in [
		"[global.rocksdb]\nchecksums = false\n",
		"[global]\nrocksdb.checksums = false\n",
		"[global]\nrocksdb = { checksums = false }\n",
		"[global.rocksdb]\n\"checksums\" = false\n",
	] {
		assert!(
			!config_from_toml(input)
				.expect("canonical input accepted")
				.rocksdb
				.checksums
		);
	}
}

#[test]
fn conflicting_file_spellings_and_scalar_ancestors_are_rejected() {
	let files = Files::new();

	for (index, document) in [
		"[global]\nrocksdb_checksums = true\n[global.rocksdb]\nchecksums = true\n",
		"[global]\nrocksdb_checksums = false\n[global.rocksdb]\nchecksums = true\n",
		"rocksdb_checksums = false\nrocksdb = 'secret sentinel'\n",
	]
	.into_iter()
	.enumerate()
	{
		let file = files.write(&format!("conflict-{index}.toml"), document);
		let error = Config::load_files([&file].into_iter())
			.expect_err("conflict rejected")
			.to_string();

		assert!(error.contains("rocksdb"));
		assert!(error.contains(&format!("conflict-{index}.toml")));
		assert!(!error.contains("secret sentinel"));
	}
}

#[test]
fn moved_type_errors_preserve_native_paths_without_values() {
	let files = Files::new();
	let old =
		files.write("old.toml", "rocksdb_checksums = 'secret sentinel'\n__aliased = 'spoof'\n");

	let new = files.write("new.toml", "[global.rocksdb]\nchecksums = 'another sentinel'\n");
	let scalar = files.write("scalar.toml", "rocksdb = 'group sentinel'\n");

	for (paths, native, source) in [
		(vec![&old], "rocksdb_checksums", "old.toml"),
		(vec![&old, &new], "rocksdb.checksums", "new.toml"),
		(vec![&scalar], "rocksdb", "scalar.toml"),
	] {
		let raw = Config::load_files(paths.into_iter()).expect("raw values accepted");
		let error = Config::new(&raw)
			.err()
			.expect("type rejected")
			.to_string();

		assert!(error.contains(native), "{error}");
		assert!(error.contains(source), "{error}");
		assert!(!error.contains("sentinel"), "{error}");
		assert!(!error.contains("spoof"), "{error}");
	}
}

#[test]
fn group_unknowns_follow_existing_strictness_policy() {
	for strict in [false, true] {
		let config = config_from_toml(&format!(
			"[global]\nerror_on_unknown_config_opts = {strict}\n[global.rocksdb]\ncheckssums = \
			 false\nruntime_extra = true\n"
		))
		.expect("unknown values retained for checking");

		let (result, logs) = check_with_captured_logs(&config);

		assert_eq!(result.is_err(), strict, "{result:?}");
		assert!(logs.contains("rocksdb.checkssums"), "{logs}");
		assert!(logs.contains("rocksdb.runtime_extra"), "{logs}");
	}
}

#[test]
fn canonical_regeneration_preserves_unknown_residue_and_is_idempotent() {
	let files = Files::new();
	let old =
		files.write("old.toml", "server_name = 'hierarchy.example'\nrocksdb_checksums = false\n");

	let extra = files.write("extra.toml", "[global.rocksdb]\nfuture_setting = ['retained']\n");
	let output = files.0.join("canonical.toml");
	let second = files.0.join("again.toml");
	let sources = Sources { paths: vec![old, extra], overrides: None };

	regenerate_config(&sources, options(&output)).expect("canonical regeneration");

	let rendered = read_to_string(&output).expect("output read");
	let raw = Config::load_files([&output].into_iter()).expect("regenerated source accepted");

	assert!(
		!Config::new(&raw)
			.expect("typed output")
			.rocksdb
			.checksums
	);

	let residue = raw
		.extract_inner::<Vec<String>>("rocksdb.future_setting")
		.unwrap();

	assert_eq!(residue, ["retained"]);

	assert!(rendered.contains("[global.rocksdb]"));
	assert!(!rendered.contains("rocksdb_checksums ="));

	let sources = Sources { paths: vec![output], overrides: None };

	regenerate_config(&sources, options(&second)).expect("second regeneration");
	assert_eq!(read_to_string(second).unwrap(), rendered);
}

#[test]
fn environment_prefixes_and_spellings_keep_precedence_and_provenance() {
	if let Ok(expected) = var(EXPECTED) {
		environment_child(&expected);
		return;
	}

	let prefixes = ["CONDUIT_", "CONDUWUIT_", "TUWUNEL_"];

	for prefix in prefixes {
		for leaf in ["ROCKSDB_CHECKSUMS", "ROCKSDB__CHECKSUMS"] {
			let name = format!("{prefix}{leaf}");

			run_environment(&[(&name, "false")], "false", &name);
		}
	}

	for (low_index, low) in prefixes.iter().enumerate() {
		for high in &prefixes[low_index.saturating_add(1)..] {
			run_prefix_pair(low, high);
		}
	}

	run_environment(
		&[("TUWUNEL_ROCKSDB_CHECKSUMS", "true"), ("TUWUNEL_ROCKSDB__CHECKSUMS", "true")],
		"conflict",
		"",
	);
}

fn run_prefix_pair(low: &str, high: &str) {
	for (older, newer) in [
		("ROCKSDB_CHECKSUMS", "ROCKSDB__CHECKSUMS"),
		("ROCKSDB__CHECKSUMS", "ROCKSDB_CHECKSUMS"),
	] {
		let low = format!("{low}{older}");
		let high = format!("{high}{newer}");

		run_environment(&[(&low, "true"), (&high, "false")], "false", &high);
	}
}

fn environment_child(expected: &str) {
	let loaded = Config::load(empty());

	if expected == "conflict" {
		let error = loaded
			.expect_err("same namespace conflict rejected")
			.to_string();

		assert!(error.contains("rocksdb_checksums"));
		assert!(error.contains("rocksdb.checksums"));
		return;
	}

	let raw = loaded.expect("environment loaded");
	let config = Config::new(&raw).expect("typed environment");
	let native = var(NATIVE).expect("native path supplied");
	let metadata = raw
		.find_metadata("rocksdb.checksums")
		.expect("winning metadata");

	assert_eq!(config.rocksdb.checksums.to_string(), expected);
	assert_eq!(metadata.interpolate(&Profile::Global, &["rocksdb", "checksums"]), native);

	if native == "TUWUNEL_ROCKSDB_CHECKSUMS" {
		let files = Files::new();
		let input =
			files.write("input.toml", "server_name = 'env.example'\nrocksdb_checksums = true\n");

		let output = files.0.join("file-only.toml");
		let included = files.0.join("included.toml");
		let sources = Sources { paths: vec![input], overrides: None };

		regenerate_config(&sources, options(&output)).expect("file-only regeneration");

		let rendered = read_to_string(&output).unwrap();
		let raw = Config::load_files([&output].into_iter()).unwrap();

		assert!(
			raw.extract_inner::<bool>("rocksdb.checksums")
				.unwrap()
		);

		assert!(rendered.contains(&native));

		let included_options = RegenerateOptions { include_env: true, ..options(&included) };

		regenerate_config(&sources, included_options).expect("environment materialization");

		let raw = Config::load_files([&included].into_iter()).unwrap();

		assert!(
			!raw.extract_inner::<bool>("rocksdb.checksums")
				.unwrap()
		);
	}
}

fn run_environment(values: &[(&str, &str)], expected: &str, native: &str) {
	let status = Command::new(current_exe().expect("test executable"))
		.arg("config::tests::hierarchy::environment_prefixes_and_spellings_keep_precedence_and_provenance")
		.env_clear().env(CHILD, "1").env(EXPECTED, expected).env(NATIVE, native)
		.envs(values.iter().copied()).status().expect("child launched");

	assert!(status.success(), "environment scenario failed");
}

fn isolated(name: &str, environment: &[(&str, &str)]) -> bool {
	const CASE: &str = "CONFIG_HIERARCHY_ISOLATED";

	if var(CASE).as_deref().eq(&Ok(name)) {
		return true;
	}

	let executable = current_exe().expect("test executable found");
	let status = vars_os()
		.map(|(name, _)| name)
		.filter(|name| {
			let name = name.to_string_lossy();
			let name = name.trim();

			ENV_PREFIXES.iter().any(|prefix| {
				name.get(..prefix.len())
					.is_some_and(|head| head.eq_ignore_ascii_case(prefix))
			})
		})
		.fold(Command::new(executable), |mut command, name| {
			command.env_remove(name);
			command
		})
		.arg(name)
		.arg(["-", "-exact"].concat())
		.env(CASE, name)
		.envs(environment.iter().copied())
		.status()
		.expect("isolated test launched");

	assert!(status.success(), "isolated test {name} failed");
	false
}

fn options(output: &Path) -> RegenerateOptions<'_> {
	RegenerateOptions {
		output: Some(output),
		force: false,
		include_env: false,
		strip_unknown: false,
	}
}

impl Drop for Files {
	fn drop(&mut self) { remove_dir_all(&self.0).expect("fixture directory removed"); }
}
