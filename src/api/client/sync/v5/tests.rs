use ruma::{OneTimeKeyAlgorithm, api::client::sync::sync_events::DeviceLists, owned_user_id};

use super::{
	Response, config_change_needs_position, is_empty_response,
	response::{E2EE, Extensions},
};

#[test]
fn unused_fallback_key_types_alone_are_empty() {
	for types in [vec![], vec![OneTimeKeyAlgorithm::SignedCurve25519]] {
		let mut alone = e2ee_response(E2EE {
			device_unused_fallback_key_types: Some(types.clone()),
			..Default::default()
		});

		assert!(is_empty_response(&mut alone));

		let e2ee = alone.extensions.e2ee;

		assert_eq!(
			e2ee.device_unused_fallback_key_types,
			Some(types.clone()),
			"the types are still delivered with the response",
		);

		let device_lists = DeviceLists {
			changed: vec![owned_user_id!("@changed:example.org")],
			..Default::default()
		};

		let mut changed = e2ee_response(E2EE {
			device_unused_fallback_key_types: Some(types.clone()),
			device_lists,
			..Default::default()
		});

		assert!(!is_empty_response(&mut changed));

		let e2ee = changed.extensions.e2ee;

		assert_eq!(
			e2ee.device_unused_fallback_key_types,
			Some(types),
			"the types are still delivered with news",
		);
	}
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
