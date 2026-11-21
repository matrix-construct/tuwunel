use super::{assert_masked, assert_verdict, config_from_toml, default_config};
use crate::config::{Config, Livekit, check, check::reload};

const COMPLETE_SECTION: &str = "[global.livekit]\nurl = \"wss://livekit.example.com\"\napi_key \
                                = \"key\"\napi_secret = \"secret\"\n";

#[test]
fn debug_masks_the_secret() {
	let config = Livekit {
		api_secret: Some("livekit-secret-sentinel".into()),
		..Default::default()
	};

	let dump = format!("{config:#?}");

	assert_masked(&dump, "api_secret", "livekit-secret-sentinel", "Some(<redacted>)");
}

/// A LiveKit section with only some of its three settings fails the check.
///
/// The refusal names every unset setting, an empty value counting as unset.
/// An absent or empty section passes and leaves the transport off; all three
/// together pass and turn it on.
#[cfg(feature = "livekit")]
#[test]
fn needs_all_three_fields() {
	const URL: &str = "wss://livekit.example.com";

	let section = |url: &str, key: &str, secret: &str| {
		format!(
			"[global.livekit]\nurl = \"{url}\"\napi_key = \"{key}\"\napi_secret = \"{secret}\"\n"
		)
	};

	for (toml, unset) in [
		("[global]\n".to_owned(), None),
		(section("", "", ""), None),
		(COMPLETE_SECTION.to_owned(), None),
		(format!("[global.livekit]\nurl = \"{URL}\"\n"), Some("api_key, api_secret")),
		(section("", "key", "secret"), Some("url")),
		(section(URL, "", "secret"), Some("api_key")),
		(section(URL, "key", ""), Some("api_secret")),
		(section("", "key", ""), Some("url, api_secret")),
		(section("", "", "secret"), Some("url, api_key")),
	] {
		let config = parse(&toml);

		assert_eq!(config.livekit.is_enabled(), toml == COMPLETE_SECTION, "{toml}");
		assert_verdict(&toml, check(&config), unset.is_none(), "livekit");

		if let Some(unset) = unset {
			let error = check(&config)
				.expect_err("a partial section fails the check")
				.to_string();

			assert!(error.contains(&format!("unset: {unset}.")), "{toml}: {error}");
		}
	}
}

#[cfg(feature = "livekit")]
#[test]
fn url_must_be_websocket() {
	for (url, valid) in [
		("wss://livekit.example.com", true),
		("ws://127.0.0.1:7880", true),
		("wss://example.com/livekit/", true),
		("https://livekit.example.com", false),
		("livekit.example.com", false),
		("wss://", false),
	] {
		let toml = COMPLETE_SECTION.replace("wss://livekit.example.com", url);

		assert_verdict(&toml, check(&parse(&toml)), valid, "livekit.url");
	}
}

/// A `url` carrying a user name or password fails the check, which never
/// quotes it.
///
/// Clients are sent the url as written, so a password in it would be
/// published.
#[cfg(feature = "livekit")]
#[test]
fn url_must_not_carry_credentials() {
	for url in [
		"wss://user:hunter2@livekit.example.com",
		"wss://hunter2@livekit.example.com",
		"wss://:hunter2@livekit.example.com",
		"https://user:hunter2@livekit.example.com",
	] {
		let toml = COMPLETE_SECTION.replace("wss://livekit.example.com", url);
		let result = check(&parse(&toml));
		let error = format!("{result:?}");

		assert!(!error.contains("hunter2"), "{url}: {error}");
		assert_verdict(&toml, result, false, "livekit.url");
	}
}

/// A reload that adds, changes or removes the LiveKit section is refused.
///
/// The routes and the advertised transport are set up at startup, so only a
/// restart may change them. An unchanged section reloads, and so does one
/// whose values are all empty, which leaves the transport off as an absent
/// section does.
#[cfg(feature = "livekit")]
#[test]
fn change_needs_a_restart() {
	let default = default_config();
	let empty = parse("[global.livekit]\nurl = \"\"\n");
	let enabled = parse(COMPLETE_SECTION);
	let rotated = parse(&COMPLETE_SECTION.replace("\"secret\"", "\"rotated\""));

	reload(&enabled, &enabled).expect("an unchanged LiveKit section should reload");
	reload(&default, &empty).expect("an empty LiveKit section should reload");

	for (old, new) in [(&default, &enabled), (&enabled, &rotated), (&enabled, &default)] {
		assert_verdict("livekit", reload(old, new), false, "livekit");
	}
}

/// A configured LiveKit section fails the check in a build without LiveKit
/// support.
///
/// The refusal names the `livekit` feature and covers startup and reload
/// alike. An absent section, or one with every value empty, passes.
#[cfg(not(feature = "livekit"))]
#[test]
fn needs_the_build_feature() {
	for (toml, valid) in [
		("[global]\n", true),
		("[global.livekit]\nurl = \"\"\n", true),
		("[global.livekit]\napi_key = \"key\"\n", false),
		(COMPLETE_SECTION, false),
	] {
		let config = parse(toml);

		assert_verdict(toml, check(&config), valid, "livekit");
		assert_verdict(toml, reload(&default_config(), &config), valid, "livekit");
	}

	let error = check(&parse(COMPLETE_SECTION))
		.expect_err("a LiveKit section should fail the check")
		.to_string();

	assert!(error.contains("\"livekit\""), "{error}");
}

fn parse(toml: &str) -> Config { config_from_toml(toml).expect("the config should parse") }
