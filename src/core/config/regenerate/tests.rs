#[cfg(target_os = "macos")]
use std::ffi::CString;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::fs::rename;
#[cfg(unix)]
use std::io::{Error as IoError, ErrorKind};
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::{
	env::{current_exe, temp_dir, var_os, vars_os},
	ffi::OsStr,
	fs::{
		OpenOptions, Permissions, create_dir_all, metadata, read_to_string, remove_dir_all,
		set_permissions, write,
	},
	io::Write as _,
	path::{Path, PathBuf},
	process::{Command, id as process_id},
	slice::from_ref,
	sync::atomic::{AtomicU64, Ordering},
};

use figment::Figment;
#[cfg(target_os = "macos")]
use libc::{getxattr, setxattr};
#[cfg(target_os = "macos")]
use smallvec::SmallVec;
use toml::{Value, from_str, value::Table};

#[cfg(unix)]
use super::write::owner_error;
#[cfg(target_os = "macos")]
use super::write::path_cstring;
use super::{
	Overwrite, RegenerateOptions, adjacent_new_path, example_config, regenerate_config,
	write::write_atomic_with_precommit, write_example_config,
};
use crate::config::{ENV_PREFIXES, Sources};

#[cfg(target_os = "macos")]
type XattrValue = SmallVec<[u8; 32]>;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const ENV_CHILD: &str = "CONFIG_REGEN_TEST_CHILD";
const ENV_INCLUDED_OUTPUT: &str = "CONFIG_REGEN_TEST_INCLUDED_OUTPUT";
const ENV_OUTPUT: &str = "CONFIG_REGEN_TEST_OUTPUT";

struct TempDir {
	path: PathBuf,
}

#[test]
fn pristine_output_matches_the_checked_in_example() {
	let expected = include_str!("../../../../tuwunel-example.toml");
	let actual = example_config().expect("pristine configuration rendered");

	assert_eq!(
		actual, expected,
		"the runtime schema and tuwunel-example.toml differ; run cargo check to regenerate the \
		 example",
	);
}

#[test]
fn regenerated_configuration_round_trips_and_is_idempotent() {
	let temp = TempDir::new("round-trip");
	let input = temp.join("input.toml");
	let first = temp.join("first.toml");
	let second = temp.join("second.toml");
	let document = concat!(
		"[global]\n",
		"server_name = \"round-trip.example\"\n",
		"port = 10443\n",
		"trusted_servers = [\"one.example\", \"two.example\"]\n",
	);

	write(&input, document).expect("input configuration written");

	let rendered = regenerate(from_ref(&input), &first);
	let parsed = parse(&rendered);
	let original = parse(&read_to_string(&input).expect("input configuration read"));

	assert_eq!(parsed, original);

	let values = global(&parsed);

	assert_eq!(values["server_name"].as_str(), Some("round-trip.example"));
	assert_eq!(values["port"].as_integer(), Some(10443));
	assert_eq!(
		values["trusted_servers"]
			.as_array()
			.expect("trusted servers array")
			.len(),
		2
	);

	let rerendered = regenerate(from_ref(&first), &second);

	assert_eq!(rerendered, rendered);
}

#[test]
fn nan_values_survive_semantic_verification() {
	let temp = TempDir::new("nan");
	let input = temp.join("input.toml");
	let output = temp.join("output.toml");
	let document = "[global]\nserver_name = \"nan.example\"\ncache_capacity_modifier = nan\n";

	write(&input, document).expect("NaN configuration written");

	let rendered = regenerate(from_ref(&input), &output);
	let parsed = parse(&rendered);
	let value = global(&parsed)["cache_capacity_modifier"]
		.as_float()
		.expect("cache capacity modifier is a float");

	assert!(value.is_nan());
}

#[test]
fn serde_aliases_are_rendered_with_canonical_names() {
	let temp = TempDir::new("aliases");
	let input = temp.join("input.toml");
	let output = temp.join("output.toml");
	let document = concat!(
		"[global]\n",
		"server_name = \"aliases.example\"\n",
		"log_colours = false\n",
		"[global.jwt]\n",
		"secret = \"legacy-jwt-key\"\n",
		"[global.storage_provider.cloud.S3]\n",
		"bucket = \"alias-bucket\"\n",
		"region = \"us-east-1\"\n",
		"startup_check = false\n",
	);

	write(&input, document).expect("aliased configuration written");

	let rendered = regenerate(from_ref(&input), &output);
	let parsed = parse(&rendered);
	let values = global(&parsed);

	assert_eq!(values["log_colors"].as_bool(), Some(false));
	assert!(!values.contains_key("log_colours"));
	assert_eq!(values["jwt"]["key"].as_str(), Some("legacy-jwt-key"));
	assert!(
		!values["jwt"]
			.as_table()
			.expect("JWT table")
			.contains_key("secret")
	);
	assert_eq!(
		values["storage_provider"]["cloud"]["s3"]["bucket"].as_str(),
		Some("alias-bucket"),
	);
	assert!(
		!values["storage_provider"]["cloud"]
			.as_table()
			.expect("storage provider table")
			.contains_key("S3")
	);
}

#[test]
fn residue_is_preserved_but_dangerous_controls_are_dropped() {
	let temp = TempDir::new("residue");
	let input = temp.join("input.toml");
	let output = temp.join("output.toml");
	let stripped = temp.join("stripped.toml");
	let document = concat!(
		"[global]\n",
		"server_name = \"residue.example\"\n",
		"unknown_future_option = \"preserved\"\n",
		"allow_invalid_tls_certificates = true\n",
		"database_restore_backup = 4\n",
		"force_migration = true\n",
		"[global.ldap]\n",
		"enable = false\n",
		"name_attribute = \"cn\"\n",
	);

	write(&input, document).expect("residue configuration written");

	let sources = Sources { paths: vec![input], overrides: None };
	let summary =
		regenerate_config(&sources, options(&output)).expect("residue configuration regenerated");

	let rendered = read_to_string(&output).expect("residue configuration read");
	let parsed = parse(&rendered);
	let values = global(&parsed);

	assert!(rendered.contains("# UNKNOWN: `unknown_future_option`"));
	assert!(rendered.contains("# UNDOCUMENTED: `allow_invalid_tls_certificates`"));
	assert!(rendered.contains("# DEPRECATED: `name_attribute`"));
	assert_eq!(values["unknown_future_option"].as_str(), Some("preserved"));
	assert_eq!(values["allow_invalid_tls_certificates"].as_bool(), Some(true));
	assert_eq!(values["ldap"]["name_attribute"].as_str(), Some("cn"));
	assert!(!values.contains_key("database_restore_backup"));
	assert!(!values.contains_key("force_migration"));
	assert!(!rendered.contains("database_restore_backup"));
	assert!(!rendered.contains("force_migration"));
	assert!(
		summary
			.dropped_keys()
			.eq(["database_restore_backup", "force_migration"])
	);

	let stripped_options = RegenerateOptions {
		output: Some(&stripped),
		force: false,
		include_env: false,
		strip_unknown: true,
	};

	regenerate_config(&sources, stripped_options).expect("stripped configuration regenerated");

	let stripped_rendered = read_to_string(stripped).expect("stripped configuration read");
	let stripped = parse(&stripped_rendered);
	let stripped_values = global(&stripped);

	assert!(!stripped_values.contains_key("unknown_future_option"));
	assert!(
		!stripped_values["ldap"]
			.as_table()
			.expect("ldap table")
			.contains_key("name_attribute")
	);
	assert_eq!(stripped_values["allow_invalid_tls_certificates"].as_bool(), Some(true));
	assert!(stripped_rendered.contains("unknown_future_option = \"preserved\""));
	assert!(stripped_rendered.contains("name_attribute = \"cn\""));
}

#[test]
fn dynamic_sections_keep_namespaces_and_repeatable_identity_providers() {
	let temp = TempDir::new("dynamic");
	let input = temp.join("input.toml");
	let output = temp.join("output.toml");
	let document = concat!(
		"[global]\n",
		"server_name = \"dynamic.example\"\n",
		"[global.well_known.support_policy.privacy]\n",
		"version = \"v1\"\n",
		"[global.well_known.support_policy.privacy.policy_translation.en]\n",
		"name = \"Privacy Policy\"\n",
		"url = \"https://dynamic.example/privacy\"\n",
		"[global.registration_terms.terms]\n",
		"version = \"1.0\"\n",
		"[global.registration_terms.terms.translations.en]\n",
		"name = \"Terms of Service\"\n",
		"url = \"https://dynamic.example/terms\"\n",
		"[[global.identity_provider]]\n",
		"brand = \"generic\"\n",
		"client_id = \"first\"\n",
		"[[global.identity_provider]]\n",
		"brand = \"generic\"\n",
		"client_id = \"second\"\n",
		"[global.storage_provider.archive.local]\n",
		"base_path = \"/srv/archive\"\n",
		"[global.storage_provider.cloud.s3]\n",
		"bucket = \"dynamic-archive\"\n",
		"region = \"us-east-1\"\n",
		"key = \"storage-key\"\n",
		"secret = \"storage-secret\"\n",
		"startup_check = false\n",
		"[global.appservice.bridge]\n",
		"url = \"http://bridge.example\"\n",
		"as_token = \"appservice-token\"\n",
		"hs_token = \"homeserver-token\"\n",
		"sender_localpart = \"bridge\"\n",
		"[[global.appservice.bridge.users]]\n",
		"exclusive = true\n",
		"regex = \"@bridge_.*:dynamic.example\"\n",
		"[global.appservice.bot]\n",
		"url = \"http://bot.example\"\n",
		"as_token = \"bot-appservice-token\"\n",
		"hs_token = \"bot-homeserver-token\"\n",
		"sender_localpart = \"bot\"\n",
		"[[global.appservice.bot.rooms]]\n",
		"exclusive = false\n",
		"regex = \"!bot_.*:dynamic.example\"\n",
		"[[global.appservice.bot.aliases]]\n",
		"exclusive = true\n",
		"regex = \"#bot_.*:dynamic.example\"\n",
	);

	write(&input, document).expect("dynamic configuration written");

	let rendered = regenerate(from_ref(&input), &output);
	let parsed = parse(&rendered);
	let values = global(&parsed);
	let providers = values["identity_provider"]
		.as_array()
		.expect("identity provider array");

	assert_eq!(providers.len(), 2);
	assert_eq!(providers[0]["client_id"].as_str(), Some("first"));
	assert_eq!(providers[1]["client_id"].as_str(), Some("second"));
	assert_eq!(
		values["well_known"]["support_policy"]["privacy"]["policy_translation"]["en"]["name"]
			.as_str(),
		Some("Privacy Policy"),
	);
	assert_eq!(
		values["registration_terms"]["terms"]["translations"]["en"]["name"].as_str(),
		Some("Terms of Service"),
	);
	assert_eq!(
		values["storage_provider"]["archive"]["local"]["base_path"].as_str(),
		Some("/srv/archive")
	);
	assert_eq!(
		values["storage_provider"]["cloud"]["s3"]["bucket"].as_str(),
		Some("dynamic-archive"),
	);
	assert_eq!(
		values["appservice"]["bridge"]["users"]
			.as_array()
			.expect("appservice user namespaces")
			.len(),
		1
	);
	assert_eq!(
		values["appservice"]["bot"]["rooms"]
			.as_array()
			.expect("appservice room namespaces")
			.len(),
		1,
	);
	assert_eq!(
		values["appservice"]["bot"]["aliases"]
			.as_array()
			.expect("appservice alias namespaces")
			.len(),
		1,
	);
}

#[test]
fn layered_inputs_require_an_output_and_collapse_in_order() {
	let temp = TempDir::new("layered");
	let base = temp.join("base.toml");
	let overlay = temp.join("overlay.toml");
	let output = temp.join("output.toml");

	write(&base, "[global]\nserver_name = \"layered.example\"\nport = 6167\n")
		.expect("base configuration written");

	write(&overlay, "[global]\nport = 7178\n").expect("overlay configuration written");

	let paths = vec![base, overlay];
	let sources = Sources { paths, overrides: None };
	let error = regenerate_config(&sources, RegenerateOptions::default())
		.expect_err("layered files need an explicit output");

	assert!(error.to_string().contains("explicit output path"));

	let summary =
		regenerate_config(&sources, options(&output)).expect("layered configuration regenerated");

	assert_eq!(summary.input_count(), 2);

	let rendered = read_to_string(output).expect("layered configuration read");
	let parsed = parse(&rendered);
	let values = global(&parsed);

	assert_eq!(values["server_name"].as_str(), Some("layered.example"));
	assert_eq!(values["port"].as_integer(), Some(7178));
}

#[test]
fn source_overrides_are_excluded_from_regeneration() {
	let temp = TempDir::new("source-overrides");
	let input = temp.join("input.toml");
	let output = temp.join("output.toml");

	write(&input, "[global]\nserver_name = \"override.example\"\nport = 6167\n")
		.expect("override input written");

	let sources = Sources {
		paths: vec![input],
		overrides: Some(Box::new(|raw: Figment| Ok(raw.merge(("port", 9199))))),
	};

	regenerate_config(&sources, options(&output))
		.expect("configuration regenerated without source override");

	let rendered = read_to_string(output).expect("override output read");

	assert_eq!(global(&parse(&rendered))["port"].as_integer(), Some(6167));
}

#[test]
fn headerless_input_is_normalized_and_named_profiles_are_rejected() {
	let temp = TempDir::new("profiles");
	let headerless = temp.join("headerless.toml");
	let named = temp.join("named.toml");
	let rejected_output = temp.join("rejected.toml");

	write(&headerless, "server_name = \"headerless.example\"\nport = 8189\n")
		.expect("headerless configuration written");

	let adjacent = adjacent_new_path(&headerless);

	let sources = Sources { paths: vec![headerless], overrides: None };

	regenerate_config(&sources, RegenerateOptions::default())
		.expect("headerless configuration regenerated");

	let rendered = read_to_string(adjacent).expect("adjacent output read");
	let parsed = parse(&rendered);

	assert_eq!(global(&parsed)["server_name"].as_str(), Some("headerless.example"));
	assert_eq!(global(&parsed)["port"].as_integer(), Some(8189));

	write(&named, "[staging]\nserver_name = \"named.example\"\n").expect("named profile written");
	let sources = Sources { paths: vec![named], overrides: None };
	let error = regenerate_config(&sources, options(&rejected_output))
		.expect_err("named profile rejected");

	assert!(
		error
			.to_string()
			.contains("Unsupported configuration profiles: staging")
	);
}

#[cfg(unix)]
#[test]
fn ownership_failure_reports_operands_and_hints_at_a_blocked_syscall() {
	const HINT: &str = "blocking the chown syscall";

	let temp = TempDir::new("owner");
	let staged = temp.join("config.toml.tmp");

	write(&staged, "").expect("staged file created");

	let owner = metadata(&staged).expect("staged metadata");
	let denied = IoError::from(ErrorKind::PermissionDenied);
	let denied = owner_error(&denied, &owner, &owner, &staged).to_string();

	assert!(denied.contains("uid"), "the message names the ownership operands");
	assert!(denied.contains(HINT), "a permission failure names the possible causes");

	let unsupported = IoError::from(ErrorKind::Unsupported);
	let unsupported = owner_error(&unsupported, &owner, &owner, &staged).to_string();

	assert!(!unsupported.contains(HINT), "the hint is limited to permission failures");
}

#[cfg(unix)]
#[test]
fn writer_uses_private_mode_and_preserves_overwritten_file() {
	let temp = TempDir::new("writer");
	let output = temp.join("config.toml");
	let backup = temp.join("config.toml.bak");
	let symlink_path = temp.join("linked.toml");

	write_example_config(&output, Overwrite::Deny).expect("new example written");

	let mode = metadata(&output)
		.expect("new output metadata")
		.permissions()
		.mode()
		& 0o777;

	assert_eq!(mode, 0o600);

	write(&output, "previous contents\n").expect("old output written");
	set_permissions(&output, Permissions::from_mode(0o640)).expect("old output mode set");
	let mut original = OpenOptions::new()
		.write(true)
		.open(&output)
		.expect("old output held open");

	write_example_config(&output, Overwrite::Deny).expect_err("replacement requires force");
	assert_eq!(read_to_string(&output).expect("refused output read"), "previous contents\n");

	write_example_config(&output, Overwrite::Allow).expect("example replaced");
	assert_eq!(read_to_string(&backup).expect("backup read"), "previous contents\n");

	original
		.set_len(0)
		.expect("displaced output truncated");

	original
		.write_all(b"changed through old descriptor\n")
		.expect("displaced output changed");

	original
		.sync_all()
		.expect("displaced output synchronized");

	let backup_contents = read_to_string(&backup).expect("independent backup read");

	assert_eq!(backup_contents, "previous contents\n");

	let mode = metadata(&output)
		.expect("replacement metadata")
		.permissions()
		.mode()
		& 0o777;

	assert_eq!(mode, 0o640);

	symlink(&output, &symlink_path).expect("output symlink created");
	write_example_config(&symlink_path, Overwrite::Allow).expect_err("symlink output refused");
}

#[cfg(target_os = "macos")]
#[test]
fn writer_preserves_extended_attributes_on_forced_replacement() {
	let temp = TempDir::new("writer-xattrs");
	let output = temp.join("config.toml");
	let backup = temp.join("config.toml.bak");
	let name = "tuwunel.test.note";
	let value = b"kept".as_slice();

	write(&output, "previous contents\n").expect("old output written");
	set_xattr(&output, name, value);

	write_example_config(&output, Overwrite::Allow).expect("example replaced");

	assert_eq!(get_xattr(&output, name).as_deref(), Some(value));
	assert_eq!(get_xattr(&backup, name).as_deref(), Some(value));
}

#[test]
fn writer_does_not_clobber_a_target_created_before_install() {
	let temp = TempDir::new("writer-new-race");
	let output = temp.join("config.toml");
	let before_commit = || {
		write(&output, "concurrent contents\n").expect("racing output written");
	};

	let result =
		write_atomic_with_precommit(&output, b"generated contents\n", false, before_commit);

	result.expect_err("racing output rejected");

	let contents = read_to_string(output).expect("racing output read");

	assert_eq!(contents, "concurrent contents\n");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn writer_rolls_back_when_a_forced_target_changes_before_commit() {
	let temp = TempDir::new("writer-replace-race");
	let output = temp.join("config.toml");
	let replacement = temp.join("replacement.toml");
	let backup = temp.join("config.toml.bak");

	write(&output, "original contents\n").expect("original output written");

	let before_commit = || {
		write(&replacement, "concurrent contents\n").expect("racing replacement written");
		rename(&replacement, &output).expect("racing replacement installed");
	};

	let result =
		write_atomic_with_precommit(&output, b"generated contents\n", true, before_commit);

	let error = result.expect_err("changed output rejected");

	assert!(error.to_string().contains("Output changed"));

	let contents = read_to_string(output).expect("restored racing output read");

	assert_eq!(contents, "concurrent contents\n");
	assert!(!backup.exists());
}

#[test]
fn config_can_be_selected_solely_through_the_path_environment() {
	let temp = TempDir::new("path-environment");
	let input = temp.join("input.toml");
	let included_output = temp.join("included.toml");
	let output = temp.join("output.toml");

	if var_os(ENV_CHILD).is_some() {
		let included_output: PathBuf = var_os(ENV_INCLUDED_OUTPUT)
			.expect("included output path set")
			.into();
		let output: PathBuf = var_os(ENV_OUTPUT)
			.expect("child output path set")
			.into();
		let sources = Sources::default();

		regenerate_config(&sources, options(&output))
			.expect("environment-selected configuration regenerated");

		let rendered = read_to_string(&output).expect("environment output read");
		let rendered_values = parse(&rendered);
		let rendered_values = global(&rendered_values);

		assert_eq!(rendered_values["server_name"].as_str(), Some("environment.example"));
		assert_eq!(rendered_values["port"].as_integer(), Some(6167));
		assert!(rendered.contains("# currently overridden by TUWUNEL_PORT."));

		let included_options = RegenerateOptions {
			output: Some(&included_output),
			force: false,
			include_env: true,
			strip_unknown: false,
		};

		regenerate_config(&sources, included_options).expect("environment values materialized");

		let included = read_to_string(included_output).expect("included output read");
		let included = parse(&included);
		let included_values = global(&included);

		assert_eq!(included_values["port"].as_integer(), Some(6555));
		assert!(!included_values.contains_key("config"));
		assert!(!included_values.contains_key("runtime_worker_threads"));
		return;
	}

	write(&input, "[global]\nserver_name = \"environment.example\"\nport = 6167\n")
		.expect("environment input written");

	let exact = ["-", "-exact"].concat();
	let executable = current_exe().expect("current test executable found");

	// env_clear() would drop the loader environment the nix check phase supplies.
	let executed = vars_os()
		.map(|(key, _)| key)
		.filter(|key| is_config_env(key))
		.fold(Command::new(executable), |mut command, key| {
			command.env_remove(key);
			command
		})
		.arg("config::regenerate::tests::config_can_be_selected_solely_through_the_path_environment")
		.arg(exact)
		.env("TUWUNEL_CONFIG", &input)
		.env("TUWUNEL_PORT", "6555")
		.env("TUWUNEL_RUNTIME_WORKER_THREADS", "23")
		.env(ENV_CHILD, "1")
		.env(ENV_INCLUDED_OUTPUT, &included_output)
		.env(ENV_OUTPUT, &output)
		.output()
		.expect("environment test child executed");

	assert!(
		executed.status.success(),
		"environment-selected child failed: {}\n{}{}",
		executed.status,
		String::from_utf8_lossy(&executed.stdout),
		String::from_utf8_lossy(&executed.stderr),
	);
}

#[cfg(target_os = "macos")]
fn set_xattr(path: &Path, name: &str, value: &[u8]) {
	let path = path_cstring(path).expect("xattr path encoded");
	let name = CString::new(name).expect("xattr name encoded");

	// SAFETY: Both strings are NUL-terminated and the value spans its length.
	let result = unsafe {
		setxattr(path.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0, 0)
	};

	assert_eq!(result, 0, "extended attribute written");
}

#[cfg(target_os = "macos")]
fn get_xattr(path: &Path, name: &str) -> Option<XattrValue> {
	let path = path_cstring(path).expect("xattr path encoded");
	let name = CString::new(name).expect("xattr name encoded");
	let mut value = [0_u8; 256];

	// SAFETY: The value buffer stays writable for the call, which bounds it by len.
	let length = unsafe {
		getxattr(path.as_ptr(), name.as_ptr(), value.as_mut_ptr().cast(), value.len(), 0, 0)
	};

	let length = usize::try_from(length).ok()?;

	value.get(..length).map(XattrValue::from_slice)
}

// Matches figment's uncased prefix filter, which trims the key first.
fn is_config_env(key: &OsStr) -> bool {
	let key = key.to_string_lossy();
	let key = key.trim();

	ENV_PREFIXES.into_iter().any(|prefix| {
		key.get(..prefix.len())
			.is_some_and(|head| head.eq_ignore_ascii_case(prefix))
	})
}

impl TempDir {
	fn new(label: &str) -> Self {
		let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
		let name = format!("tuwunel-config-regenerator-{label}-{}-{sequence}", process_id());
		let path = temp_dir().join(name);

		create_dir_all(&path).expect("temporary directory created");

		Self { path }
	}

	fn join(&self, name: &str) -> PathBuf { self.path.join(name) }
}

impl Drop for TempDir {
	fn drop(&mut self) { remove_dir_all(&self.path).ok(); }
}

fn regenerate(paths: &[PathBuf], output: &Path) -> String {
	let sources = Sources { paths: paths.to_vec(), overrides: None };

	regenerate_config(&sources, options(output)).expect("configuration regenerated");
	read_to_string(output).expect("regenerated configuration read")
}

fn options(output: &Path) -> RegenerateOptions<'_> {
	RegenerateOptions {
		output: Some(output),
		force: false,
		include_env: false,
		strip_unknown: false,
	}
}

fn parse(document: &str) -> Value { from_str(document).expect("configuration parses as TOML") }

fn global(document: &Value) -> &Table {
	document
		.get("global")
		.and_then(Value::as_table)
		.expect("global table present")
}
