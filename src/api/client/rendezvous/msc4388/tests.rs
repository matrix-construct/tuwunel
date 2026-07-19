use axum::{body::to_bytes, response::IntoResponse};
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header::CONTENT_TYPE};
use ruma::api::error::ErrorKind;

use super::{
	Error, SEC_FETCH_DEST, SEC_FETCH_MODE, SEC_FETCH_SITE, SEC_FETCH_USER, data_to_string,
	ensure_safe_get, max_data_bytes,
};

#[test]
fn payload_limit_never_exceeds_msc_limit() {
	assert_eq!(max_data_bytes(1_024), 1_024);
	assert_eq!(max_data_bytes(usize::MAX), 4_096);
}

#[test]
fn safe_get_accepts_non_navigation_fetches() {
	let mut headers = HeaderMap::new();
	ensure_safe_get(&headers).expect("missing fetch metadata should be accepted");

	headers.insert(SEC_FETCH_DEST, HeaderValue::from_static("empty"));
	headers.insert(SEC_FETCH_MODE, HeaderValue::from_static("cors"));
	headers.insert(SEC_FETCH_SITE, HeaderValue::from_static("same-origin"));
	headers.insert(SEC_FETCH_USER, HeaderValue::from_static("?0"));
	ensure_safe_get(&headers).expect("non-navigation fetch should be accepted");
}

#[test]
fn safe_get_rejects_navigation_fetches() {
	for (name, value) in [
		(SEC_FETCH_DEST, "document"),
		(SEC_FETCH_MODE, "navigate"),
		(SEC_FETCH_SITE, "none"),
		(SEC_FETCH_USER, "?1"),
	] {
		let headers = HeaderMap::from_iter([(
			HeaderName::from_static(name),
			HeaderValue::from_static(value),
		)]);
		let error = ensure_safe_get(&headers).expect_err("navigation fetch should be rejected");

		assert_eq!(error.kind(), ErrorKind::forbidden());
		assert_eq!(error.status_code(), StatusCode::FORBIDDEN);
	}
}

#[test]
fn rendezvous_data_must_be_utf8() {
	assert_eq!(
		data_to_string(&Bytes::from_static(b"valid UTF-8"))
			.expect("valid UTF-8 should be accepted"),
		"valid UTF-8",
	);

	let error = data_to_string(&Bytes::from_static(b"\xFF"))
		.expect_err("invalid UTF-8 should be rejected");

	assert_eq!(error.kind(), ErrorKind::Unknown);
	assert_eq!(error.status_code(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn concurrent_write_uses_unstable_matrix_error() {
	let response = Error::ConcurrentWrite.into_response();

	assert_eq!(response.status(), StatusCode::CONFLICT);
	assert_eq!(response.headers()[CONTENT_TYPE], "application/json");

	let body = to_bytes(response.into_body(), usize::MAX)
		.await
		.expect("response body should be readable");

	assert_eq!(
		body.as_ref(),
		br#"{"errcode":"IO_ELEMENT_MSC4388_CONCURRENT_WRITE","error":"sequence_token does not match"}"#,
	);
}
