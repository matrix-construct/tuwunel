use ruma::{
	api::appservice::{Namespaces, Registration, RegistrationInit},
	server_name,
};
use tuwunel_core::Result;

use super::{
	IS_JOINED_SCOPE, RegistrationInfo, Registrations, destination, receivers, socket_path,
};

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
