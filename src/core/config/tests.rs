#![cfg(test)]

use std::{
	cell::RefCell,
	io::{Result as IoResult, Write},
	sync::{Arc, Mutex, Once},
};

use tracing::{level_filters::LevelFilter, subscriber::set_global_default};
use tracing_subscriber::fmt::{MakeWriter, fmt};

use super::*;
use crate::utils::BoolExt;

thread_local! {
	static CAPTURE: RefCell<Option<Arc<Mutex<Vec<u8>>>>> = const { RefCell::new(None) };
}

struct ThreadLocalWriter;

impl Write for ThreadLocalWriter {
	fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
		CAPTURE.with_borrow(|sink| {
			if let Some(sink) = sink {
				sink.lock()
					.expect("buffer lock poisoned")
					.extend_from_slice(buf);
			}
		});

		Ok(buf.len())
	}

	fn flush(&mut self) -> IoResult<()> { Ok(()) }
}

impl<'a> MakeWriter<'a> for ThreadLocalWriter {
	type Writer = Self;

	fn make_writer(&'a self) -> Self::Writer { Self }
}

fn config_from_toml(toml: &str) -> Result<Config> {
	Config::new(&Figment::new().merge(Data::nested(Toml::string(toml))))
}

fn check_with_captured_logs(config: &Config) -> (Result, String) {
	static INIT: Once = Once::new();

	// Installed once, process-wide for the whole test binary, since a per-test
	// set_default races tracing's interest cache; future capture tests reuse this.
	INIT.call_once(|| {
		let subscriber = fmt()
			.with_ansi(false)
			.with_max_level(LevelFilter::INFO)
			.with_writer(ThreadLocalWriter)
			.finish();

		set_global_default(subscriber).ok();
	});

	let captured = Arc::new(Mutex::new(Vec::new()));
	CAPTURE.with_borrow_mut(|sink| *sink = Some(Arc::clone(&captured)));

	let result = check(config);
	CAPTURE.with_borrow_mut(|sink| *sink = None);

	let logs = String::from_utf8(
		captured
			.lock()
			.expect("buffer lock poisoned")
			.clone(),
	)
	.expect("captured tracing output should be valid UTF-8");

	(result, logs)
}

#[test]
fn ip_source_absent_parses_as_none() {
	let config = config_from_toml("[global]\n").unwrap();

	assert_eq!(config.ip_source, None);
}

#[test]
fn ip_source_connect_info_parses() {
	let config = config_from_toml(
		r#"[global]
ip_source = "connect_info"
"#,
	)
	.unwrap();

	assert_eq!(config.ip_source, Some(IpSource::ConnectInfo));
}

#[test]
fn ip_source_rightmost_x_forwarded_for_parses() {
	let config = config_from_toml(
		r#"[global]
ip_source = "rightmost_x_forwarded_for"
"#,
	)
	.unwrap();

	assert_eq!(config.ip_source, Some(IpSource::RightmostXForwardedFor));
}

#[test]
fn ip_source_cf_connecting_ip_parses() {
	let config = config_from_toml(
		r#"[global]
ip_source = "cf_connecting_ip"
"#,
	)
	.unwrap();

	assert_eq!(config.ip_source, Some(IpSource::CfConnectingIp));
}

#[test]
fn ip_source_issue_427_values_parse() {
	for (value, expected) in [
		("connect_info", IpSource::ConnectInfo),
		("rightmost_x_forwarded_for", IpSource::RightmostXForwardedFor),
		("rightmost_forwarded", IpSource::RightmostForwarded),
		("x_real_ip", IpSource::XRealIp),
		("cf_connecting_ip", IpSource::CfConnectingIp),
		("true_client_ip", IpSource::TrueClientIp),
		("fly_client_ip", IpSource::FlyClientIp),
		("cloudfront_viewer_address", IpSource::CloudFrontViewerAddress),
	] {
		let config = config_from_toml(&format!(
			r#"[global]
ip_source = "{value}"
"#,
		))
		.unwrap();

		assert_eq!(config.ip_source, Some(expected), "{value}");
	}
}

#[test]
fn ip_source_camel_case_and_bogus_fail_to_parse() {
	for value in ["CamelCase", "bogus"] {
		let result = config_from_toml(&format!(
			r#"[global]
ip_source = "{value}"
"#,
		));

		let Err(err) = result else {
			panic!("ip_source value {value:?} should fail to parse");
		};

		let err = err.to_string();
		assert!(err.contains("ip_source"), "{err}");
		assert!(err.contains(value), "{err}");
	}
}

#[test]
fn check_accepts_absent_connect_info_and_cf_connecting_ip() {
	let absent = config_from_toml("[global]\n").unwrap();
	let connect_info = config_from_toml(
		r#"[global]
ip_source = "connect_info"
"#,
	)
	.unwrap();
	let cf_connecting_ip = config_from_toml(
		r#"[global]
ip_source = "cf_connecting_ip"
"#,
	)
	.unwrap();

	let (result, logs) = check_with_captured_logs(&absent);
	result.expect("absent ip_source should pass config check");
	assert!(!logs.contains("ip_source is set to"));

	let (result, logs) = check_with_captured_logs(&connect_info);
	result.expect("connect_info should pass config check");
	assert!(!logs.contains("ip_source is set to"));

	let (result, logs) = check_with_captured_logs(&cf_connecting_ip);
	result.expect("cf_connecting_ip should pass config check");
	assert!(logs.contains("ip_source is set to CfConnectingIp"));
}

#[test]
fn reload_rejects_none_to_some_and_some_to_none() {
	let none = config_from_toml("[global]\n").unwrap();
	let some = config_from_toml(
		r#"[global]
ip_source = "connect_info"
"#,
	)
	.unwrap();
	let other_some = config_from_toml(
		r#"[global]
ip_source = "rightmost_x_forwarded_for"
"#,
	)
	.unwrap();

	let err = check::reload(&none, &some).unwrap_err();
	assert!(
		err.to_string().contains("'ip_source'")
			&& err
				.to_string()
				.contains("cannot be changed at runtime"),
		"{err}"
	);

	let err = check::reload(&some, &none).unwrap_err();
	assert!(
		err.to_string().contains("'ip_source'")
			&& err
				.to_string()
				.contains("cannot be changed at runtime"),
		"{err}"
	);

	let err = check::reload(&some, &other_some).unwrap_err();
	assert!(
		err.to_string().contains("'ip_source'")
			&& err
				.to_string()
				.contains("cannot be changed at runtime"),
		"{err}"
	);
}

#[test]
fn s3_storage_provider_debug_masks_credentials() {
	let config = StorageProviderS3 {
		key: Some("AKIAIOSFODNN7EXAMPLE".to_owned()),
		secret: Some("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_owned()),
		token: Some("session-token".to_owned()),
		kms: Some("kms-material".to_owned()),
		..Default::default()
	};

	let dump = format!("{config:?}");

	assert!(!dump.contains("AKIAIOSFODNN7EXAMPLE"), "key leaked in Debug: {dump}");
	assert!(!dump.contains("wJalrXUtnFEMI"), "secret leaked in Debug: {dump}");
	assert!(!dump.contains("session-token"), "token leaked in Debug: {dump}");
	assert!(!dump.contains("kms-material"), "kms leaked in Debug: {dump}");

	for field in ["key", "secret", "token", "kms"] {
		assert!(
			dump.contains(&format!("{field}: Some(<redacted>)")),
			"{field} should appear as Some(<redacted>): {dump}"
		);
	}
}

#[test]
fn reload_accepts_unchanged_none_and_unchanged_some() {
	let none = config_from_toml("[global]\n").unwrap();
	let some = config_from_toml(
		r#"[global]
ip_source = "rightmost_x_forwarded_for"
"#,
	)
	.unwrap();

	check::reload(&none, &none).expect("unchanged none config should reload");
	check::reload(&some, &some).expect("unchanged some config should reload");
}

fn check_support_pgp_key(value: &str) -> Result {
	let toml = format!(
		"[global.well_known.support_contact.admin]\nrole = \"m.role.admin\"\nemail_address = \
		 \"admin@example.com\"\npgp_key = \"{value}\"\n"
	);
	let config = config_from_toml(&toml).expect("support_contact config should parse");
	check_with_captured_logs(&config).0
}

#[test]
fn pgp_key_accepts_any_uri_scheme() {
	for value in [
		"https://example.com/key.asc",
		"openpgp4fpr:8B77919975EAFA5E2456EE03665FE73077489DB0",
		"dns:HASH._openpgpkey.example.com?type=OPENPGPKEY",
	] {
		check_support_pgp_key(value)
			.unwrap_or_else(|e| panic!("`{value}` should be accepted as a pgp_key: {e}"));
	}
}

#[test]
fn pgp_key_rejects_raw_material_and_bare_fingerprints() {
	let err = check_support_pgp_key("8B77919975EAFA5E2456EE03665FE73077489DB0").unwrap_err();
	assert!(err.to_string().contains("openpgp4fpr"), "{err}");

	let err = check_support_pgp_key("-----BEGIN PGP PUBLIC KEY BLOCK-----").unwrap_err();
	assert!(err.to_string().contains("inlined key material"), "{err}");

	let err = check_support_pgp_key("openpgp4fpr:nothex").unwrap_err();
	assert!(err.to_string().contains("hex fingerprint"), "{err}");
}

#[test]
fn default_power_level_content_override_accepts_a_table() {
	let config = config_from_toml(
		"[global]
[global.default_power_level_content_override]
users_default = 50
",
	)
	.expect("a table value parses");

	check(&config)
		.expect("a table default_power_level_content_override should pass config check");
}

#[test]
fn default_power_level_content_override_rejects_a_non_table() {
	let config = config_from_toml(
		"[global]
default_power_level_content_override = false
",
	)
	.expect("a scalar value parses into config");

	let err = check(&config)
		.expect_err("a non-table default_power_level_content_override must be rejected")
		.to_string();
	assert!(err.contains("default_power_level_content_override"), "{err}");
}

/// A documented default is published to operators through the generated
/// tuwunel-example.toml, so one that disagrees with the code hands out a value
/// the server never uses. Only integer defaults are compared; prose such as
/// "varies by system" and non-literal bodies are out of reach here.
#[test]
fn documented_defaults_match_the_code() {
	const SRC: &str = include_str!("mod.rs");
	const SERDE_DEFAULT: &str = "#[serde(default = \"";

	let lines: Vec<&str> = SRC.lines().collect();
	let bodies: BTreeMap<&str, &str> = lines
		.iter()
		.copied()
		.filter_map(default_fn_body)
		.collect();

	let compared: Vec<(&str, u64, u64)> = lines
		.iter()
		.enumerate()
		.filter_map(|(at, line)| {
			let name = line
				.trim()
				.strip_prefix(SERDE_DEFAULT)?
				.split('"')
				.next()?;

			let documented = documented_default(&lines, at)?;
			let actual = bodies.get(name).copied().and_then(eval_int)?;

			Some((name, documented, actual))
		})
		.collect();

	let mismatched: Vec<String> = compared
		.iter()
		.filter(|(_, documented, actual)| documented != actual)
		.map(|(name, documented, actual)| {
			format!("{name}: documented {documented}, code returns {actual}")
		})
		.collect();

	assert!(
		mismatched.is_empty(),
		"documented defaults disagree with the code:\n  {}",
		mismatched.join("\n  ")
	);

	assert!(
		compared.len() >= 70,
		"only {} integer defaults were compared; the source parsing above has stopped matching \
		 and this test is no longer checking anything",
		compared.len()
	);
}

/// A one-line `fn default_x() -> T { body }`, as (`default_x`, `body`).
fn default_fn_body(line: &str) -> Option<(&str, &str)> {
	let rest = line.trim().strip_prefix("fn ")?;
	let (name, rest) = rest.split_once("()")?;
	let body = rest.split_once('{')?.1.rsplit_once('}')?.0;

	name.starts_with("default_")
		.then_some((name, body.trim()))
}

/// The nearest `/// default: N` in the doc comment above `at`.
fn documented_default(lines: &[&str], at: usize) -> Option<u64> {
	lines[..at]
		.iter()
		.rev()
		.take_while(|line| {
			let line = line.trim();
			line.starts_with("///") || line.starts_with("#[")
		})
		.find_map(|line| line.trim().strip_prefix("/// default: "))
		.and_then(leading_int)
}

/// The leading integer, ignoring any trailing prose such as "86400 (24 hours)".
fn leading_int(text: &str) -> Option<u64> {
	let end = text
		.find(|c: char| !c.is_ascii_digit())
		.unwrap_or(text.len());

	text.split_at(end).0.parse().ok()
}

/// Integer literals joined by `+` and `*`; a call, float or bool yields None.
fn eval_int(body: &str) -> Option<u64> {
	body.split('+')
		.map(|term| {
			term.split('*')
				.map(int_literal)
				.try_fold(1_u64, |acc, factor| acc.checked_mul(factor?))
		})
		.try_fold(0_u64, |acc, term| acc.checked_add(term?))
}

/// `1024_u16`, `60`, `10_000`; anything else yields None.
fn int_literal(token: &str) -> Option<u64> {
	const INT_SUFFIX: [&str; 11] =
		["", "u8", "u16", "u32", "u64", "usize", "i8", "i16", "i32", "i64", "isize"];

	let token = token.trim();
	let digits: String = token
		.chars()
		.take_while(|c| c.is_ascii_digit() || *c == '_')
		.filter(char::is_ascii_digit)
		.collect();

	let suffix = token.trim_start_matches(|c: char| c.is_ascii_digit() || c == '_');

	INT_SUFFIX
		.contains(&suffix)
		.and_then(|| digits.parse().ok())
}
