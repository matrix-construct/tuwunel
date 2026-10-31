mod aggregate;
mod aliases;
mod compatibility;
mod deployments;
mod groups;
mod loading;
mod regeneration;
mod residue;
mod secrets;

use std::{
	env::{current_exe, temp_dir, var},
	fs::{create_dir_all, read_to_string, remove_dir_all, write},
	iter::empty,
	path::{Path, PathBuf},
	process::{Command, id},
	sync::atomic::{AtomicU64, Ordering},
};

use figment::Profile;
use tuwunel_macros::implement;

use super::config_from_toml;
use crate::{
	Result,
	config::{Config, ENV_PREFIXES, RegenerateOptions, Sources, regenerate_config},
	utils::sys::command_without_env_prefixes,
};

static NEXT: AtomicU64 = AtomicU64::new(0);
const CHILD: &str = "CONFIG_HIERARCHY_CHILD";
const EXPECTED: &str = "CONFIG_HIERARCHY_EXPECTED";
const NATIVE: &str = "CONFIG_HIERARCHY_NATIVE";

struct Files(PathBuf);

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
fn type_errors_preserve_native_paths_and_omit_secret_values() {
	let files = Files::new();
	let old =
		files.write("old.toml", "registration_token = 9876543210123\n__aliased = 'spoof'\n");

	let new = files.write("new.toml", "[global.registration]\ntoken = 9876543210456\n");
	let scalar = files.write("scalar.toml", "registration = 'group sentinel'\n");
	let jwt = files.write("jwt.toml", "[global.jwt]\nkey = 9876543210789\n");

	for (paths, native, source) in [
		(vec![&old], "registration_token", "old.toml"),
		(vec![&old, &new], "registration.token", "new.toml"),
		(vec![&scalar], "registration", "scalar.toml"),
		(vec![&jwt], "jwt.key", "jwt.toml"),
	] {
		let raw = Config::load_files(paths.into_iter()).expect("raw values accepted");
		let error = rejection(Config::new(&raw));

		assert!(error.contains(native), "{error}");
		assert!(error.contains(source), "{error}");
		assert!(!error.contains("9876543210"), "{error}");
		assert!(!error.contains("sentinel"), "{error}");
		assert!(!error.contains("spoof"), "{error}");
	}
}

#[track_caller]
fn rejection<T>(result: Result<T>) -> String {
	result
		.err()
		.expect("configuration rejected")
		.to_string()
}

#[test]
fn type_errors_keep_the_detail_of_ordinary_values() {
	let error = rejection(config_from_toml("[global]\nrocksdb_checksums = 'yes'\n"));

	assert!(error.contains("expected a boolean"), "{error}");
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

	for prefix in prefixes {
		for value in ["true", "false"] {
			let flat = format!("{prefix}ROCKSDB_CHECKSUMS");
			let nested = format!("{prefix}ROCKSDB__CHECKSUMS");

			run_environment(&[(&flat, "true"), (&nested, value)], "conflict", "");
		}
	}
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

		let startup = Config::load([input.as_path()].into_iter()).expect("file and env loaded");

		assert!(
			!Config::new(&startup)
				.expect("startup config extracts")
				.rocksdb
				.checksums
		);

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
	let output = environment_command()
		.arg("config::tests::hierarchy::environment_prefixes_and_spellings_keep_precedence_and_provenance")
		.env(CHILD, "1").env(EXPECTED, expected).env(NATIVE, native)
		.envs(values.iter().copied()).output().expect("child launched");

	assert!(
		output.status.success(),
		"environment scenario failed: {}\n{}{}",
		output.status,
		String::from_utf8_lossy(&output.stdout),
		String::from_utf8_lossy(&output.stderr),
	);
}

fn environment_command() -> Command {
	// Preserve the loader environment required by the Nix check phase.
	let executable = current_exe().expect("test executable found");
	let command = Command::new(executable);

	command_without_env_prefixes(command, &ENV_PREFIXES)
}

fn isolated(name: &str, environment: &[(&str, &str)]) -> bool {
	const CASE: &str = "CONFIG_HIERARCHY_ISOLATED";

	if var(CASE).as_deref().eq(&Ok(name)) {
		return true;
	}

	let status = environment_command()
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
