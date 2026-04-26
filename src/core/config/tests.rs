#![cfg(test)]

use std::{
	io::{Result as IoResult, Write},
	sync::{Arc, Mutex},
};

use tracing_subscriber::fmt::MakeWriter;

use super::*;

#[derive(Clone)]
struct SharedBufferWriter(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBufferWriter {
	fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
		self.0
			.lock()
			.expect("buffer lock poisoned")
			.write(buf)
	}

	fn flush(&mut self) -> IoResult<()> { Ok(()) }
}

impl<'a> MakeWriter<'a> for SharedBufferWriter {
	type Writer = Self;

	fn make_writer(&'a self) -> Self::Writer { self.clone() }
}

fn config_from_toml(toml: &str) -> Result<Config> {
	Config::new(&Figment::new().merge(Data::nested(Toml::string(toml))))
}

fn check_with_captured_logs(config: &Config) -> (Result, String) {
	let captured = Arc::new(Mutex::new(Vec::new()));
	let subscriber = tracing_subscriber::fmt()
		.with_ansi(false)
		.with_writer(SharedBufferWriter(Arc::clone(&captured)))
		.finish();

	let result = {
		let _guard = tracing::subscriber::set_default(subscriber);
		check(config)
	};

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
