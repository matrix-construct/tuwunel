use ruma::{
	api::appservice::{Namespaces, Registration, RegistrationInit},
	server_name,
};
use tuwunel_core::Result;

use super::{
	IS_JOINED_SCOPE, RegistrationInfo, Registrations, claim_holder, destination,
	proxy::{Root, split_path},
	proxy_claim, receivers, socket_path,
};

const LIVEKIT_URL: &str = "http://lk-jwt-service:8080";

#[test]
fn socket_urls() {
	for (url, expected) in [
		("unix:///run/a.sock", Some("/run/a.sock")),
		("unix:/run/a.sock", Some("/run/a.sock")),
		("UNIX:///run/a.sock", Some("/run/a.sock")),
		("unix:///run/my%20dir/a.sock", Some("/run/my%20dir/a.sock")),
		("http://127.0.0.1:1234", None),
	] {
		assert_eq!(socket_path(url).unwrap(), expected, "{url}");
	}

	for url in ["unix://run/a.sock", "unix:run/a.sock"] {
		assert!(socket_path(url).is_err(), "{url}");
	}
}

#[test]
fn destination_is_the_url_unless_absent_null_or_empty() {
	for (url, expected) in [
		(None, None),
		(Some("null"), None),
		(Some(""), None),
		(Some("http://127.0.0.1:1234"), Some("http://127.0.0.1:1234")),
		(Some("unix:///run/a.sock"), Some("unix:///run/a.sock")),
	] {
		let registration = registration(url, &[]);

		assert_eq!(destination(&registration), expected, "{url:?}");
	}
}

#[test]
fn senders_skip_a_registration_without_a_url() {
	let registrations: Registrations = [
		(None, "absent"),
		(Some("null"), "null"),
		(Some(""), "empty"),
		(Some("http://127.0.0.1:1234"), "bridge"),
	]
	.into_iter()
	.map(|(url, id)| {
		let info = registration_info(url, &[]).expect("valid registration");

		(id.to_owned(), info)
	})
	.collect();

	let urls: Vec<_> = receivers(&registrations)
		.map(|info| info.registration.url.as_deref())
		.collect();

	assert_eq!(urls, [Some("http://127.0.0.1:1234")]);
}

#[test]
fn known_scope_is_admitted() {
	let info = registration_info(None, &[IS_JOINED_SCOPE]).expect("known scope");

	assert_eq!(info.unknown_scope(), None);
	assert!(info.has_scope(IS_JOINED_SCOPE));
	assert!(!info.has_destination());
}

#[test]
fn unknown_scope_is_reported() {
	let stable = "urn:matrix:client:rooms:is_joined";
	let info = registration_info(None, &[IS_JOINED_SCOPE, stable]).expect("loads anyway");

	assert_eq!(info.unknown_scope(), Some(stable), "the stable scope is not known yet");
	assert!(info.has_scope(IS_JOINED_SCOPE), "the known scope is still granted");
}

#[test]
fn scope_is_granted_only_when_requested() {
	let info = registration_info(Some("http://127.0.0.1:1234"), &[]).expect("no scopes");

	assert!(!info.has_scope(IS_JOINED_SCOPE));
	assert!(info.has_destination());
}

#[test]
fn proxy_claim_keys_come_in_pairs() {
	assert_eq!(proxy_claim(&proxied(None, None)).unwrap(), None);

	for (prefix, url) in [(Some("rtc/livekit"), None), (None, Some(LIVEKIT_URL))] {
		assert!(proxy_claim(&proxied(prefix, url)).is_err(), "{prefix:?} {url:?}");
	}
}

#[test]
fn proxy_claim_prefix_must_be_allowed_exactly() {
	for prefix in ["rtc/livekit", "rtc/livekit/"] {
		let claim = proxy_claim(&proxied(Some(prefix), Some(LIVEKIT_URL))).unwrap();

		assert_eq!(claim, Some("rtc/livekit"), "{prefix}");
	}

	for prefix in [
		"",
		"/",
		"rtc",
		"rtc/",
		"rtc/livekit-2",
		"rtc/livekit/sub",
		"send",
		"/rtc/livekit",
	] {
		assert!(proxy_claim(&proxied(Some(prefix), Some(LIVEKIT_URL))).is_err(), "{prefix}");
	}
}

#[test]
fn proxy_claim_url_must_be_http_or_a_socket() {
	for url in ["", "/", "unix://run/a.sock", "ftp://lk-jwt-service", "lk-jwt-service:8080"] {
		assert!(proxy_claim(&proxied(Some("rtc/livekit"), Some(url))).is_err(), "{url}");
	}

	for url in ["unix:///run/lk.sock", "https://lk-jwt-service", LIVEKIT_URL] {
		let claim = proxy_claim(&proxied(Some("rtc/livekit"), Some(url))).unwrap();

		assert_eq!(claim, Some("rtc/livekit"), "{url}");
	}
}

#[test]
fn invalid_proxy_claim_loads_without_it() {
	let registration = proxied(Some("send"), Some(LIVEKIT_URL));
	let info = RegistrationInfo::new(registration, server_name!("a.test")).expect("loads anyway");

	assert_eq!(info.proxy_prefix, None);
	assert_eq!(info.proxy_url(), None);
	assert!(!info.proxy_covers("send"));
}

#[test]
fn valid_proxy_claim_covers_its_prefix_and_beneath() {
	let registration = proxied(Some("rtc/livekit/"), Some("http://lk-jwt-service:8080/"));
	let info = RegistrationInfo::new(registration, server_name!("a.test")).expect("valid claim");

	assert_eq!(info.proxy_url(), Some(LIVEKIT_URL));

	for remainder in ["rtc/livekit", "rtc/livekit/", "rtc/livekit/get_token"] {
		assert!(info.proxy_covers(remainder), "{remainder}");
	}

	for remainder in
		["", "rtc", "rtc/", "rtc/livekit-2", "rtc/livekitx/get_token", "x/rtc/livekit"]
	{
		assert!(!info.proxy_covers(remainder), "{remainder}");
	}

	assert_eq!(info.without_proxy_claim().proxy_url(), None);
}

#[test]
fn proxy_claim_holder_is_another_registration() {
	let claimed = |id: &str| {
		let registration = Registration {
			id: id.to_owned(),
			..proxied(Some("rtc/livekit"), Some(LIVEKIT_URL))
		};

		RegistrationInfo::new(registration, server_name!("a.test")).expect("valid claim")
	};

	let registrations: Registrations = [("first".to_owned(), claimed("first"))].into();

	assert_eq!(claim_holder(&registrations, &claimed("second")), Some("first"));
	assert_eq!(claim_holder(&registrations, &claimed("first")), None, "re-registering");

	let unclaimed = registration_info(None, &[]).expect("no claim");

	assert_eq!(claim_holder(&registrations, &unclaimed), None);
}

#[test]
fn proxy_paths_split_after_the_version() {
	for (path, expected) in [
		(
			"/_matrix/client/v1/rtc/livekit/get_token",
			Some((Root::Client, "rtc/livekit/get_token")),
		),
		(
			"/_matrix/client/unstable/io.element.msc4195/rtc/livekit",
			Some((Root::Client, "rtc/livekit")),
		),
		(
			"/_matrix/federation/v1/rtc/livekit/get_token",
			Some((Root::Federation, "rtc/livekit/get_token")),
		),
		("/_matrix/federation/v1/", Some((Root::Federation, ""))),
		("/_matrix/client/rtc/livekit/get_token", None),
		("/_matrix/client/v/rtc/livekit", None),
		("/_matrix/client/unstable//rtc/livekit", None),
		("/_matrix/client/unstable/rtc", None),
		("/_matrix/client/v1", None),
		("/_matrix/media/v1/rtc/livekit", None),
		("/_matrix/client", None),
	] {
		assert_eq!(split_path(path), expected, "{path}");
	}
}

fn proxied(prefix: Option<&str>, url: Option<&str>) -> Registration {
	Registration {
		proxy_prefix: prefix.map(ToOwned::to_owned),
		proxy_url: url.map(ToOwned::to_owned),
		..registration(None, &[])
	}
}

fn registration_info(url: Option<&str>, scopes: &[&str]) -> Result<RegistrationInfo> {
	RegistrationInfo::new(registration(url, scopes), server_name!("a.test"))
}

fn registration(url: Option<&str>, scopes: &[&str]) -> Registration {
	let base: Registration = RegistrationInit {
		id: "bridge".to_owned(),
		url: url.map(ToOwned::to_owned),
		as_token: "as_token".to_owned(),
		hs_token: "hs_token".to_owned(),
		sender_localpart: "bridge_bot".to_owned(),
		namespaces: Namespaces::new(),
		rate_limited: None,
		protocols: None,
	}
	.into();

	let scopes = scopes
		.iter()
		.copied()
		.map(ToOwned::to_owned)
		.collect();

	Registration { scopes, ..base }
}
