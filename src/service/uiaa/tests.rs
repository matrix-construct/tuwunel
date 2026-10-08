use std::thread::scope;

use ruma::{CanonicalJsonValue, api::client::uiaa::UiaaInfo, device_id, user_id};
use serde_json::json;
use tuwunel_core::{Result, config::Figment};

use super::{MAX_REQUEST_BYTES, MAX_REQUESTS, Service};
use crate::test_utils::fixture;

#[tokio::test]
async fn request_bodies_are_bounded_and_released() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let uiaa = &fixture.services.uiaa;
	let user_id = user_id!("@alice:localhost");
	let device_id = device_id!("ALICEDEVICE");

	let open = |session: &str, pad: usize| {
		let info = UiaaInfo {
			session: Some(session.to_owned()),
			..Default::default()
		};

		let body = json!({ "devices": [], "pad": "x".repeat(pad) });
		let body = CanonicalJsonValue::try_from(body).expect("canonical request body");

		uiaa.create(user_id, device_id, &info, &body);
	};

	let retained = |session: &str| {
		uiaa.get_uiaa_request(user_id, Some(device_id), session)
			.is_some()
	};

	for session in 0..=MAX_REQUESTS {
		open(&session.to_string(), 0);
	}

	assert!(!retained("0"), "the least recently used body is dropped");
	assert!(retained("1"));
	open("recent", 0);
	assert!(!retained("2"), "reading a body promotes its recency");
	assert!(retained("1"));

	open("oversized", MAX_REQUEST_BYTES);
	assert!(!retained("oversized"), "an oversized body is not kept");

	uiaa.update_uiaa_session(user_id, device_id, "1", None);
	assert!(!retained("1"), "a finished session releases its body");

	byte_boundaries(uiaa);
	structured_and_concurrent(uiaa);

	Ok(())
}

fn byte_boundaries(uiaa: &Service) {
	let user_id = user_id!("@alice:localhost");
	let device_id = device_id!("ALICEDEVICE");
	let payload = MAX_REQUEST_BYTES
		.saturating_sub(user_id.as_str().len())
		.saturating_sub(device_id.as_str().len())
		.saturating_sub("exact".len())
		.saturating_sub(2);

	let exact = CanonicalJsonValue::String("x".repeat(payload));
	let over = CanonicalJsonValue::String("x".repeat(payload.saturating_add(1)));

	uiaa.set_uiaa_request(user_id, device_id, "exact", &exact);
	uiaa.set_uiaa_request(user_id, device_id, "large", &over);
	assert_eq!(uiaa.get_uiaa_request(user_id, Some(device_id), "exact"), Some(exact));
	assert!(
		uiaa.get_uiaa_request(user_id, Some(device_id), "large")
			.is_none()
	);

	let device = "x".repeat(MAX_REQUEST_BYTES);
	let device = device.as_str().into();
	let small = CanonicalJsonValue::Null;

	uiaa.set_uiaa_request(user_id, device, "key", &small);
	assert!(
		uiaa.get_uiaa_request(user_id, Some(device), "key")
			.is_none()
	);

	let session = "x".repeat(MAX_REQUEST_BYTES);

	uiaa.set_uiaa_request(user_id, device_id, &session, &small);
	assert!(
		uiaa.get_uiaa_request(user_id, Some(device_id), &session)
			.is_none()
	);
}

fn structured_and_concurrent(uiaa: &Service) {
	let user_id = user_id!("@alice:localhost");
	let device_id = device_id!("ALICEDEVICE");
	let body = json!({
		"master_key": {
			"user_id": user_id,
			"usage": ["master"],
			"keys": {"ed25519:key": "key"}
		},
		"nested": [[null, {}, [], true, 0]]
	});

	let body = CanonicalJsonValue::try_from(body).expect("canonical body");

	uiaa.set_uiaa_request(user_id, device_id, "structured", &body);
	assert_eq!(
		uiaa.get_uiaa_request(user_id, Some(device_id), "structured"),
		Some(body.clone())
	);

	let body = &body;

	scope(|scope| {
		for worker in 0..8 {
			let _worker = scope.spawn(move || insert_concurrently(uiaa, worker, body));
		}
	});

	assert_eq!(
		uiaa.userdevicesessionid_uiaarequest
			.lock()
			.expect("locked")
			.len(),
		MAX_REQUESTS
	);
}

fn insert_concurrently(uiaa: &Service, worker: usize, body: &CanonicalJsonValue) {
	let user_id = user_id!("@alice:localhost");
	let device_id = device_id!("ALICEDEVICE");

	for sequence in 0..256 {
		let session = format!("parallel-{worker}-{sequence}");

		uiaa.set_uiaa_request(user_id, device_id, &session, body);
		assert!(
			uiaa.userdevicesessionid_uiaarequest
				.lock()
				.expect("locked")
				.len()
				.le(&MAX_REQUESTS)
		);
	}
}
