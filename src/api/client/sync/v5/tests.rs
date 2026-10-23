use std::time::Duration;

use ruma::{OneTimeKeyAlgorithm, api::client::sync::sync_events::DeviceLists, owned_user_id};

use super::{
	ListId, Request, Response, config_change_needs_position, is_empty_response, poll_timeout,
	response::{E2EE, Extensions},
};

#[test]
fn unused_fallback_key_types_alone_are_empty() {
	for types in [vec![], vec![OneTimeKeyAlgorithm::SignedCurve25519]] {
		let alone = E2EE {
			device_unused_fallback_key_types: Some(types.clone()),
			..Default::default()
		};

		assert!(is_empty_keeping_types(alone, false));

		let device_lists = DeviceLists {
			changed: vec![owned_user_id!("@changed:example.org")],
			..Default::default()
		};

		let changed = E2EE {
			device_unused_fallback_key_types: Some(types),
			device_lists,
			..Default::default()
		};

		assert!(!is_empty_keeping_types(changed, false));
	}
}

#[test]
fn unused_fallback_key_types_answer_a_woken_pass() {
	for types in [vec![], vec![OneTimeKeyAlgorithm::SignedCurve25519]] {
		let alone = E2EE {
			device_unused_fallback_key_types: Some(types),
			..Default::default()
		};

		assert!(!is_empty_keeping_types(alone, true));
	}

	assert!(
		is_empty_keeping_types(E2EE::default(), true),
		"a woken pass without news is empty"
	);
}

#[test]
fn list_counts_alone_are_empty() {
	let counts_only = || Response {
		lists: [(ListId::from("all_rooms"), Default::default())].into(),
		..Response::new(Default::default())
	};

	assert!(is_empty_response(&mut counts_only(), false));
	assert!(is_empty_response(&mut counts_only(), true));
}

fn is_empty_keeping_types(e2ee: E2EE, woken: bool) -> bool {
	let types = e2ee.device_unused_fallback_key_types.clone();
	let mut response = e2ee_response(e2ee);
	let empty = is_empty_response(&mut response, woken);

	assert_eq!(
		response
			.extensions
			.e2ee
			.device_unused_fallback_key_types,
		types,
		"the types are still delivered",
	);

	empty
}

fn e2ee_response(e2ee: E2EE) -> Response {
	Response {
		extensions: Extensions { e2ee, ..Default::default() },
		..Response::new(Default::default())
	}
}

#[test]
fn caught_up_advancing_config_change_needs_position() {
	assert!(config_change_needs_position(true, true, true, 1));
	assert!(!config_change_needs_position(false, true, true, 1));
	assert!(!config_change_needs_position(true, false, true, 1));
	assert!(!config_change_needs_position(true, true, false, 1));
	assert!(!config_change_needs_position(true, true, true, 0));
}

#[test]
fn opening_request_ignores_timeout() {
	let request = |pos: Option<&str>| Request {
		pos: pos.map(Into::into),
		timeout: Some(Duration::from_secs(30)),
		..Request::new()
	};

	assert_eq!(poll_timeout(&request(None), 90_000), 0);
	assert_eq!(poll_timeout(&request(Some("1")), 90_000), 30_000);
	assert_eq!(poll_timeout(&request(Some("1")), 10_000), 10_000);
}
