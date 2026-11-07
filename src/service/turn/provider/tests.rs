//! Tests for folding provider answers into Matrix TURN credentials.
//!
//! The answers are the shapes the providers document: the TURN REST API,
//! Cloudflare's ICE server list, and a bare list with string `urls`. The
//! coverage also pins the TURN-only and port 53 filters, the requested-lifetime
//! cap, the answers rejected as unusable, and the endpoint query string.

use std::time::Duration;

use serde_json::{Value, from_value, json};

use super::{Answer, uses_port_53, with_query};

const DAY: Duration = Duration::from_hours(24);

#[test]
fn rest_answer_keeps_its_turn_uris() {
	let rest = json!({
		"username": "12334939:mbzrxpgjys",
		"password": "adfsaflsjfldssia",
		"ttl": 3600,
		"uris": [
			"turn:1.2.3.4:9991?transport=udp",
			"stun:1.2.3.4:3478",
			"turns:1.2.3.4:443?transport=tcp",
		],
	});

	let credentials = answer(rest)
		.into_credentials(DAY, |_| true)
		.expect("credentials");

	assert_eq!(credentials.username, "12334939:mbzrxpgjys");
	assert_eq!(credentials.password, "adfsaflsjfldssia");
	assert_eq!(credentials.ttl, Duration::from_hours(1));
	assert_eq!(credentials.uris, [
		"turn:1.2.3.4:9991?transport=udp",
		"turns:1.2.3.4:443?transport=tcp"
	]);
}

#[test]
fn rest_lifetime_is_cut_to_the_request() {
	let rest = json!({ "username": "u", "password": "p", "ttl": 999_999, "uris": ["turn:a"] });
	let credentials = answer(rest)
		.into_credentials(DAY, |_| true)
		.expect("credentials");

	assert_eq!(credentials.ttl, DAY);
}

#[test]
fn cloudflare_answer_keeps_turn_without_port_53() {
	let cloudflare = json!({
		"iceServers": [
			{ "urls": ["stun:stun.cloudflare.com:3478", "stun:stun.cloudflare.com:53"] },
			{
				"urls": [
					"turn:turn.cloudflare.com:3478?transport=udp",
					"turn:turn.cloudflare.com:53?transport=udp",
					"turns:turn.cloudflare.com:443?transport=tcp",
				],
				"username": "bc91b63e",
				"credential": "ebd71f1d",
			},
		],
	});

	let credentials = answer(cloudflare)
		.into_credentials(DAY, |uri| !uses_port_53(uri))
		.expect("credentials");

	assert_eq!(credentials.username, "bc91b63e");
	assert_eq!(credentials.password, "ebd71f1d");
	assert_eq!(credentials.ttl, DAY, "an ICE answer takes the requested lifetime");
	assert_eq!(credentials.uris, [
		"turn:turn.cloudflare.com:3478?transport=udp",
		"turns:turn.cloudflare.com:443?transport=tcp",
	]);
}

#[test]
fn bare_list_with_string_urls_is_accepted() {
	let servers = json!([
		{ "urls": "stun:relay.example:80" },
		{ "urls": "turn:relay.example:80", "username": "u", "credential": "p" },
		{ "urls": "turns:relay.example:443?transport=tcp", "username": "u", "credential": "p" },
	]);

	let credentials = answer(servers)
		.into_credentials(DAY, |_| true)
		.expect("credentials");

	assert_eq!(credentials.uris, [
		"turn:relay.example:80",
		"turns:relay.example:443?transport=tcp"
	]);
}

#[test]
fn entries_with_other_credentials_or_none_are_trimmed() {
	let servers = json!({
		"iceServers": [
			{ "urls": ["turn:open.example"] },
			{ "urls": ["turn:first.example"], "username": "a", "credential": "1" },
			{ "urls": ["turn:second.example"], "username": "b", "credential": "2" },
		],
	});

	let credentials = answer(servers)
		.into_credentials(DAY, |_| true)
		.expect("credentials");

	assert_eq!(credentials.username, "a");
	assert_eq!(credentials.uris, ["turn:first.example"]);
}

#[test]
fn answers_clients_cannot_use_are_errors() {
	let unusable = [
		("STUN only", json!({ "iceServers": [{ "urls": ["stun:stun.example"] }] })),
		("no URIs", json!({ "username": "u", "password": "p", "ttl": 60, "uris": [] })),
		(
			"no lifetime",
			json!({ "username": "u", "password": "p", "ttl": 0, "uris": ["turn:a"] }),
		),
		(
			"all filtered",
			json!({ "iceServers": [{ "urls": ["turn:a:53"], "username": "u", "credential": "p" }] }),
		),
	];

	for (case, value) in unusable {
		let rejected = answer(value)
			.into_credentials(DAY, |uri| !uses_port_53(uri))
			.is_err();

		assert!(rejected, "{case}");
	}

	let shapeless = json!({ "username": "u", "password": "p", "ttl": 60 });

	assert!(from_value::<Answer>(shapeless).is_err(), "no answer shape matches");
}

#[test]
fn empty_credentials_count_as_none() {
	let servers = json!({
		"iceServers": [
			{ "urls": ["stun:stun.example", "turn:open.example"], "username": "", "credential": "" },
			{ "urls": ["turn:relay.example"], "username": "u", "credential": "p" },
		],
	});

	let credentials = answer(servers)
		.into_credentials(DAY, |_| true)
		.expect("credentials");

	assert_eq!(credentials.username, "u");
	assert_eq!(credentials.uris, ["turn:relay.example"]);
}

#[test]
fn port_53_is_found_with_or_without_a_query() {
	assert!(uses_port_53("turn:turn.cloudflare.com:53?transport=udp"));
	assert!(uses_port_53("turn:turn.cloudflare.com:53"));
	assert!(uses_port_53("turn:[2001:db8::1]:53?transport=tcp"));
	assert!(!uses_port_53("turn:turn.cloudflare.com:5349?transport=tcp"));
	assert!(!uses_port_53("turn:turn.cloudflare.com"));
	assert!(!uses_port_53("turn:[2001:db8::53]"));
}

#[test]
fn endpoint_query_keeps_existing_parameters() {
	let url = "https://relay.example/api/v1/turn/credentials?apiKey=k"
		.parse()
		.expect("url");

	assert_eq!(
		with_query(url, DAY).as_str(),
		"https://relay.example/api/v1/turn/credentials?apiKey=k&service=turn&ttl=86400"
	);
}

fn answer(value: Value) -> Answer { from_value(value).expect("answer should deserialize") }
