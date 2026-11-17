use std::iter::once;

use url::Url;

use super::{check_with_captured_logs, config_from_toml};

type Case<'a> = (&'a str, Option<&'a str>, &'a [Option<&'a str>], bool);
type Warned<'a> = (&'a str, Option<&'a str>, &'a [Option<&'a str>], (bool, bool));

const BASE: &str = "https://matrix.example";
const LEGACY: &str = "https://matrix.example/_matrix/client/unstable/login/sso/callback/a";
const PREFIXED: &str =
	"https://example.org:8448/matrix/_matrix/client/unstable/login/sso/callback/a";

const MOVED: &str = "https://matrix.example/_tuwunel/sso/callback/a";
const LOOKALIKE: &str = "https://matrix.example/x_matrix/client/unstable/login/sso/callback/a";
const DEEPER: &str = "https://matrix.example/_matrix/client/unstable/login/sso/callback/a/b";
const LEGACY_WARNING: &str = "uses the legacy path";
const REGISTERED_WARNING: &str = "returns users to the redirect URI it has registered";

#[test]
fn legacy_callback_mounts_while_a_provider_sends_users_there() {
	let cases: [Case<'_>; 11] = [
		("no providers", Some(BASE), &[], false),
		("explicit legacy", Some(BASE), &[Some(LEGACY)], true),
		("explicit legacy behind a prefix", Some(BASE), &[Some(PREFIXED)], true),
		("explicit new", Some(BASE), &[Some(MOVED)], false),
		("explicit new without well_known", None, &[Some(MOVED)], false),
		("derived", Some(BASE), &[None], false),
		("registered", None, &[None], true),
		("lookalike path", Some(BASE), &[Some(LOOKALIKE)], false),
		("deeper path", Some(BASE), &[Some(DEEPER)], false),
		("new beside registered", None, &[Some(MOVED), None], true),
		("new beside derived", Some(BASE), &[Some(MOVED), None], false),
	];

	for (name, client, callbacks, mounted) in cases {
		let config = config_from_toml(&providers_toml(client, callbacks)).expect(name);

		assert_eq!(config.mounts_legacy_sso_callback(), mounted, "{name}");
	}
}

fn providers_toml(client: Option<&str>, callbacks: &[Option<&str>]) -> String {
	let well_known = client
		.map(|client| format!("[global.well_known]\nclient = \"{client}\"\n"))
		.unwrap_or_default();

	let providers = callbacks.iter().enumerate().map(|(i, callback)| {
		let callback = callback
			.map(|url| format!("callback_url = \"{url}\"\n"))
			.unwrap_or_default();

		format!(
			"[[global.identity_provider]]\nbrand = \"keycloak\"\nclient_id = \"idp{i}\"\n\
			 client_secret = \"secret\"\nissuer_url = \"https://sso.example\"\n{callback}"
		)
	});

	once(format!("[global]\n{well_known}"))
		.chain(providers)
		.collect()
}

#[test]
fn moved_callback_url_keeps_the_host_prefix_and_provider() {
	let cases = [
		(LEGACY, Some(MOVED)),
		(PREFIXED, Some("https://example.org:8448/matrix/_tuwunel/sso/callback/a")),
		(MOVED, None),
		(LOOKALIKE, None),
		(DEEPER, None),
	];

	for (callback, moved) in cases {
		let config = config_from_toml(&providers_toml(None, &[Some(callback)])).expect(callback);
		let provider = config
			.identity_provider
			.values()
			.next()
			.expect("one provider");

		let replacement = provider.moved_callback_url();

		assert_eq!(replacement.as_ref().map(Url::as_str), moved, "{callback}");
	}
}

#[test]
fn startup_warns_for_each_provider_on_the_legacy_route() {
	let cases: [Warned<'_>; 4] = [
		("explicit legacy", Some(BASE), &[Some(LEGACY)], (true, false)),
		("registered", None, &[None], (false, true)),
		("explicit new", None, &[Some(MOVED)], (false, false)),
		("derived", Some(BASE), &[None], (false, false)),
	];

	for (name, client, callbacks, (pinned, registered)) in cases {
		let config = config_from_toml(&providers_toml(client, callbacks)).expect(name);
		let (result, logs) = check_with_captured_logs(&config);

		result.expect(name);
		assert_eq!(logs.contains(LEGACY_WARNING), pinned, "{name}");
		assert_eq!(logs.contains(MOVED), pinned, "{name} names the new URL");
		assert_eq!(logs.contains(REGISTERED_WARNING), registered, "{name}");
	}
}
