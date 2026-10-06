//! Tests the request bodies kept for pending UIAA sessions.

use ruma::{CanonicalJsonValue, api::client::uiaa::UiaaInfo, device_id, user_id};
use serde_json::json;
use tuwunel_core::{Result, config::Figment};

use super::{MAX_REQUEST_BYTES, MAX_REQUESTS};
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

	open("oversized", MAX_REQUEST_BYTES);
	assert!(!retained("oversized"), "an oversized body is not kept");

	uiaa.update_uiaa_session(user_id, device_id, "1", None);
	assert!(!retained("1"), "a finished session releases its body");

	Ok(())
}
