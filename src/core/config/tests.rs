#![cfg(test)]

use std::{
	cell::RefCell,
	io::{Result as IoResult, Write},
	sync::{Arc, Mutex, Once},
};

use figment::providers::Data;
use tracing::{level_filters::LevelFilter, subscriber::set_global_default};
use tracing_subscriber::fmt::{MakeWriter, fmt};

use super::*;
use crate::{
	config::proxy::{ProxySnapshot, parse_environment_proxy_url},
	utils::BoolExt,
};

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
fn check_warns_when_mas_provisioning_provider_is_untrusted() {
	let untrusted_provisioning = r#"[global]
mas_secret = "provisioning-secret"

[[global.identity_provider]]
brand = "MAS"
client_id = "mas"
client_secret = "oauth-secret"
"#;

	let trusted_provisioning = r#"[global]
mas_secret = "provisioning-secret"

[[global.identity_provider]]
brand = "MAS"
client_id = "mas"
client_secret = "oauth-secret"
trusted = true
"#;

	let untrusted_login = r#"[global]

[[global.identity_provider]]
brand = "MAS"
client_id = "mas"
client_secret = "oauth-secret"
"#;

	let cases = [
		("untrusted provisioning provider", untrusted_provisioning, true),
		("trusted provisioning provider", trusted_provisioning, false),
		("untrusted login-only provider", untrusted_login, false),
	];

	for (name, toml, warns) in cases {
		let config = config_from_toml(toml).expect("MAS provider config should parse");
		let (result, logs) = check_with_captured_logs(&config);

		result.expect("MAS provider config should pass config check");
		assert_eq!(logs.contains("configured without `trusted = true`"), warns, "{name}");
	}
}

#[test]
fn check_warns_when_oidc_registration_token_is_set() {
	let token_unset = "[global]\n";

	let token_empty = r#"[global]
oidc_registration_access_token = ""
"#;

	let token_set = r#"[global]
oidc_registration_access_token = "initial-access-token"
"#;

	let cases = [
		("token unset", token_unset, false),
		("token empty", token_empty, false),
		("token set", token_set, true),
	];

	for (name, toml, warns) in cases {
		let config = config_from_toml(toml).expect("OIDC registration config should parse");
		let (result, logs) = check_with_captured_logs(&config);
		let secret = config.oidc_registration_access_token.as_str();

		result.expect("OIDC registration config should pass config check");
		assert_eq!(logs.contains("oidc_registration_access_token is set"), warns, "{name}");
		assert!(secret.is_empty() || !logs.contains(secret), "{name} leaked the token value");
	}
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

#[test]
fn proxy_none_has_no_configured_surface() {
	let config = config_from_toml("[global]\nproxy = \"none\"\n").expect("proxy config parses");
	let url = Url::parse("https://example.com/").expect("test URL parses");

	assert_eq!(config.proxy.hosts().count(), 0);
	assert!(!config.proxy.intercepts(&url));
}

#[test]
fn global_proxy_exposes_its_host_and_intercepts_http_urls() {
	let config = config_from_toml(
		"[global.proxy]\nglobal = { url = \"socks5h://proxy.internal:1080\" }\n",
	)
	.expect("proxy config parses");

	let http = Url::parse("http://example.com/").expect("test URL parses");
	let https = Url::parse("https://example.com/").expect("test URL parses");
	let ftp = Url::parse("ftp://example.com/").expect("test URL parses");

	assert_eq!(config.proxy.hosts().collect::<Vec<_>>(), ["proxy.internal"]);
	assert!(config.proxy.intercepts(&http));
	assert!(config.proxy.intercepts(&https));
	assert!(!config.proxy.intercepts(&ftp));
}

#[test]
fn domain_proxy_uses_the_configured_rule_for_both_surfaces() {
	let config = config_from_toml(
		"[global.proxy]\n[[global.proxy.by_domain]]\nurl = \
		 \"socks5h://proxy.internal:1080\"\ninclude = [\"*.example.com\"]\nexclude = \
		 [\"private.example.com\"]\n",
	)
	.expect("proxy config parses");

	let included = Url::parse("https://public.example.com/").expect("test URL parses");
	let excluded = Url::parse("https://private.example.com/").expect("test URL parses");
	let unrelated = Url::parse("https://example.org/").expect("test URL parses");

	assert_eq!(config.proxy.hosts().collect::<Vec<_>>(), ["proxy.internal"]);
	assert!(config.proxy.intercepts(&included));
	assert!(!config.proxy.intercepts(&excluded));
	assert!(!config.proxy.intercepts(&unrelated));
}

#[test]
fn environment_proxy_snapshot_preserves_scheme_and_no_proxy_rules() {
	let proxy = proxy_snapshot(&ProxyConfig::None, false, &[
		("HTTP_PROXY", "http://proxy.internal:8080"),
		("NO_PROXY", "EXAMPLE.COM,10.0.0.0/8,192.0.2.1,2001:db8::1,."),
	]);

	let http = Url::parse("http://matrix.org/").expect("test URL parses");
	let https = Url::parse("https://matrix.org/").expect("test URL parses");
	let domain_bypass = Url::parse("http://sub.example.com/").expect("test URL parses");
	let cidr_bypass = Url::parse("http://10.1.2.3/").expect("test URL parses");
	let ipv4_bypass = Url::parse("http://192.0.2.1/").expect("test URL parses");
	let ipv6_bypass = Url::parse("http://[2001:db8::1]/").expect("test URL parses");
	let trailing_dot_bypass = Url::parse("http://matrix.org./").expect("test URL parses");

	assert_eq!(proxy.hosts().collect::<Vec<_>>(), ["proxy.internal"]);
	assert!(proxy.intercepts(&http));
	assert!(!proxy.intercepts(&https));
	assert!(!proxy.intercepts(&domain_bypass));
	assert!(!proxy.intercepts(&cidr_bypass));
	assert!(!proxy.intercepts(&ipv4_bypass));
	assert!(!proxy.intercepts(&ipv6_bypass));
	assert!(!proxy.intercepts(&trailing_dot_bypass));
}

fn proxy_snapshot(config: &ProxyConfig, is_cgi: bool, vars: &[(&str, &str)]) -> ProxySnapshot {
	ProxySnapshot::with_vars(config, is_cgi, |name| {
		vars.iter()
			.find_map(|(key, value)| (*key == name).then(|| (*value).to_owned()))
	})
	.expect("proxy snapshot builds")
}

#[test]
fn environment_proxy_precedence_and_fallback_match_the_http_client() {
	let proxy = proxy_snapshot(&ProxyConfig::None, false, &[
		("HTTP_PROXY", "socks4://upper.internal:1080"),
		("http_proxy", "socks5h://lower.internal:1080"),
		("HTTPS_PROXY", "ftp://invalid.internal:21"),
		("https_proxy", "https://ignored.internal:8443"),
		("ALL_PROXY", "socks4a://fallback.internal:1080"),
		("all_proxy", "socks5://ignored-all.internal:1080"),
		("NO_PROXY", ".EXAMPLE.COM"),
		("no_proxy", "matrix.org"),
	]);

	let http = Url::parse("http://matrix.org/").expect("test URL parses");
	let https = Url::parse("https://matrix.org/").expect("test URL parses");
	let bypass = Url::parse("https://sub.example.com/").expect("test URL parses");
	let local_alias = Url::parse("http://upper.internal/").expect("test URL parses");
	let remote_alias = Url::parse("https://fallback.internal/").expect("test URL parses");

	assert_eq!(proxy.hosts().collect::<Vec<_>>(), ["upper.internal", "fallback.internal"]);
	assert!(proxy.intercepts(&http));
	assert!(proxy.intercepts(&https));
	assert!(!proxy.intercepts(&bypass));
	assert!(proxy.resolver_alias(&local_alias));
	assert!(!proxy.resolver_alias(&remote_alias));
}

#[test]
fn environment_proxy_uri_metadata_matches_the_http_client_parser() {
	let is_cgi = false;

	for (proxy_url, proxy_host, endpoint, resolver_alias) in [
		(
			"socks5://no-port.internal",
			"no-port.internal",
			"https://no-port.internal/",
			true,
		),
		(
			"http://odd-port.internal:notaport",
			"odd-port.internal",
			"https://odd-port.internal/",
			false,
		),
		("http://[v1.proxy]", "v1.proxy", "https://v1.proxy/", false),
	] {
		let proxy = proxy_snapshot(&ProxyConfig::None, is_cgi, &[("ALL_PROXY", proxy_url)]);
		let request = Url::parse("https://matrix.org/").expect("test URL parses");
		let endpoint = Url::parse(endpoint).expect("test URL parses");

		assert_eq!(proxy.hosts().collect::<Vec<_>>(), [proxy_host]);
		assert!(proxy.intercepts(&request));
		assert_eq!(proxy.resolver_alias(&endpoint), resolver_alias);
		assert_eq!(
			parse_environment_proxy_url(proxy_url).and_then(|url| url.port_or_known_default()),
			Some(80),
		);
	}
}

#[test]
fn environment_proxy_credentials_preserve_empty_passwords() {
	for proxy_url in ["http://Aladdin@proxy.internal", "socks5://Aladdin:@proxy.internal"] {
		let proxy =
			parse_environment_proxy_url(proxy_url).expect("credentialed proxy URL parses");

		assert_eq!(proxy.username(), "Aladdin");
		assert_eq!(proxy.password(), None);
	}

	let proxy = parse_environment_proxy_url("http://user:password@proxy.internal")
		.expect("credentialed proxy URL parses");

	assert_eq!(proxy.username(), "user");
	assert_eq!(proxy.password(), Some("password"));
}

#[test]
fn environment_proxy_ip_endpoints_do_not_enter_dns_exemptions() {
	let is_cgi = false;

	for proxy_url in ["http://192.0.2.1:8080", "http://[2001:db8::1]:8080"] {
		let proxy = proxy_snapshot(&ProxyConfig::None, is_cgi, &[("ALL_PROXY", proxy_url)]);
		let request = Url::parse("https://matrix.org/").expect("test URL parses");

		assert_eq!(proxy.hosts().count(), 0);
		assert!(proxy.intercepts(&request));
	}
}

#[test]
fn lowercase_environment_proxy_variables_are_supported() {
	let proxy = proxy_snapshot(&ProxyConfig::None, false, &[
		("http_proxy", "socks5://http.internal:1080"),
		("https_proxy", "socks5h://https.internal:1080"),
		("no_proxy", "example.com"),
	]);
	let fallback = proxy_snapshot(&ProxyConfig::None, false, &[(
		"all_proxy",
		"socks4a://all.internal:1080",
	)]);

	let http = Url::parse("http://matrix.org/").expect("test URL parses");
	let https = Url::parse("https://matrix.org/").expect("test URL parses");
	let bypass = Url::parse("https://sub.example.com/").expect("test URL parses");

	assert_eq!(proxy.hosts().collect::<Vec<_>>(), ["http.internal", "https.internal"]);
	assert!(proxy.intercepts(&http));
	assert!(proxy.intercepts(&https));
	assert!(!proxy.intercepts(&bypass));
	assert_eq!(fallback.hosts().collect::<Vec<_>>(), ["all.internal"]);
	assert!(fallback.intercepts(&http));
	assert!(fallback.intercepts(&https));
}

#[test]
fn environment_all_proxy_deduplicates_the_endpoint_host() {
	let proxy =
		proxy_snapshot(&ProxyConfig::None, false, &[("ALL_PROXY", "proxy.internal:8080")]);

	let http = Url::parse("http://matrix.org/").expect("test URL parses");
	let https = Url::parse("https://matrix.org/").expect("test URL parses");

	assert_eq!(proxy.hosts().collect::<Vec<_>>(), ["proxy.internal"]);
	assert!(proxy.intercepts(&http));
	assert!(proxy.intercepts(&https));
}

#[test]
fn environment_proxy_snapshot_is_disabled_for_cgi() {
	let proxy = proxy_snapshot(&ProxyConfig::None, true, &[(
		"ALL_PROXY",
		"socks5h://proxy.internal:1080",
	)]);

	let url = Url::parse("https://example.com/").expect("test URL parses");

	assert_eq!(proxy.hosts().count(), 0);
	assert!(!proxy.intercepts(&url));
}

#[test]
fn resolver_alias_recognizes_direct_endpoint_destinations() {
	let config = config_from_toml(
		"[global.proxy]\n[[global.proxy.by_domain]]\nurl = \
		 \"socks5h://proxy.internal:1080\"\ninclude = [\"*.example.com\"]\n",
	)
	.expect("proxy config parses");

	let proxy = proxy_snapshot(&config.proxy, false, &[]);
	let alias = Url::parse("https://proxy.internal/").expect("test URL parses");
	let proxied = Url::parse("https://public.example.com/").expect("test URL parses");
	let unrelated = Url::parse("https://example.org/").expect("test URL parses");

	assert!(proxy.resolver_alias(&alias));
	assert!(!proxy.resolver_alias(&proxied));
	assert!(!proxy.resolver_alias(&unrelated));
}

#[test]
fn resolver_alias_tracks_the_selected_dns_mode() {
	let alias = Url::parse("https://proxy.internal/").expect("test URL parses");

	for (scheme, expected) in [
		("socks4", true),
		("socks5", true),
		("socks4a", false),
		("socks5h", false),
		("http", false),
		("https", false),
	] {
		let toml =
			format!("[global.proxy]\nglobal = {{ url = \"{scheme}://proxy.internal:1080\" }}\n");

		let config = config_from_toml(&toml).expect("proxy config parses");
		let proxy = proxy_snapshot(&config.proxy, false, &[]);

		assert_eq!(proxy.resolver_alias(&alias), expected, "proxy scheme {scheme}");
	}
}

#[test]
fn resolver_alias_uses_the_first_matching_domain_proxy() {
	let local_first = config_from_toml(
		"[global.proxy]\n[[global.proxy.by_domain]]\nurl = \
		 \"socks5://proxy.internal:1080\"\n[[global.proxy.by_domain]]\nurl = \
		 \"socks5h://proxy.internal:1080\"\n",
	)
	.expect("proxy config parses");

	let remote_first = config_from_toml(
		"[global.proxy]\n[[global.proxy.by_domain]]\nurl = \
		 \"socks5h://proxy.internal:1080\"\n[[global.proxy.by_domain]]\nurl = \
		 \"socks5://proxy.internal:1080\"\n",
	)
	.expect("proxy config parses");

	let alias = Url::parse("https://proxy.internal/").expect("test URL parses");

	let local = proxy_snapshot(&local_first.proxy, false, &[]);
	let remote = proxy_snapshot(&remote_first.proxy, false, &[]);

	assert!(local.resolver_alias(&alias));
	assert!(!remote.resolver_alias(&alias));
}

#[test]
fn explicit_proxy_suppresses_environment_proxy_rules() {
	let config = config_from_toml(
		"[global.proxy]\n[[global.proxy.by_domain]]\nurl = \
		 \"socks5h://configured.internal:1080\"\ninclude = [\"*.example.com\"]\n",
	)
	.expect("proxy config parses");

	let proxy = proxy_snapshot(&config.proxy, false, &[(
		"ALL_PROXY",
		"http://environment.internal:8080",
	)]);

	let unrelated = Url::parse("https://example.org/").expect("test URL parses");

	assert_eq!(proxy.hosts().collect::<Vec<_>>(), ["configured.internal"]);
	assert!(!proxy.intercepts(&unrelated));
}

#[test]
fn environment_no_proxy_star_bypasses_domains_but_not_ip_addresses() {
	let proxy = proxy_snapshot(&ProxyConfig::None, false, &[
		("ALL_PROXY", "http://proxy.internal:8080"),
		("NO_PROXY", "*"),
	]);

	let domain = Url::parse("https://example.com/").expect("test URL parses");
	let ip = Url::parse("https://127.0.0.1/").expect("test URL parses");

	assert_eq!(proxy.hosts().collect::<Vec<_>>(), ["proxy.internal"]);
	assert!(!proxy.intercepts(&domain));
	assert!(proxy.intercepts(&ip));
}

#[test]
fn environment_no_proxy_endpoint_is_a_resolver_alias() {
	let proxy = proxy_snapshot(&ProxyConfig::None, false, &[
		("HTTP_PROXY", "http://proxy.internal:8080"),
		("NO_PROXY", "proxy.internal"),
	]);

	let url = Url::parse("http://proxy.internal/").expect("test URL parses");

	assert!(proxy.resolver_alias(&url));
}

#[test]
fn unsupported_configured_proxy_scheme_is_rejected() {
	for toml in [
		"[global.proxy]\nglobal = { url = \"ftp://proxy.internal:21\" }\n",
		"[global.proxy]\n[[global.proxy.by_domain]]\nurl = \"ftp://proxy.internal:21\"\ninclude \
		 = [\"*\"]\n",
	] {
		let config = config_from_toml(toml).expect("proxy config parses");
		let url = Url::parse("https://example.com/").expect("test URL parses");

		assert!(!config.proxy.intercepts(&url));
		config
			.proxy
			.to_proxy()
			.expect_err("unsupported proxy scheme must fail");
		assert!(ProxySnapshot::with_vars(&config.proxy, false, |_| None).is_err());
	}
}

#[test]
fn proxy_snapshots_own_their_configured_and_environment_generations() {
	let original_url = Url::parse("socks5://configured.internal:1080").expect("test URL parses");
	let mut configured = ProxyConfig::Global { url: original_url };
	let configured_snapshot = proxy_snapshot(&configured, false, &[]);

	configured = ProxyConfig::None;
	assert!(matches!(configured, ProxyConfig::None));

	let configured_url = Url::parse("https://configured.internal/").expect("test URL parses");
	assert_eq!(configured_snapshot.hosts().collect::<Vec<_>>(), ["configured.internal"]);
	assert!(configured_snapshot.intercepts(&configured_url));
	assert!(configured_snapshot.resolver_alias(&configured_url));

	let environment_url = RefCell::new("socks5://environment.internal:1080");
	let environment_snapshot = ProxySnapshot::with_vars(&ProxyConfig::None, false, |name| {
		(name == "ALL_PROXY").then(|| environment_url.borrow().to_string())
	})
	.expect("proxy snapshot builds");

	*environment_url.borrow_mut() = "http://changed.internal:8080";

	let environment_url = Url::parse("https://environment.internal/").expect("test URL parses");
	assert_eq!(environment_snapshot.hosts().collect::<Vec<_>>(), ["environment.internal"]);
	assert!(environment_snapshot.intercepts(&environment_url));
	assert!(environment_snapshot.resolver_alias(&environment_url));
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
