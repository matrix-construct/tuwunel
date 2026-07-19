mod create;
mod delete;
mod get;
mod msc4388;
mod put;

use axum::{
	body::{Body, to_bytes},
	response::Response,
};
use bytes::Bytes;
use http::{
	HeaderMap, HeaderValue, StatusCode,
	header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, ETAG, EXPIRES, LAST_MODIFIED, PRAGMA},
};
use ruma::http_headers::system_time_to_http_date;
use tuwunel_core::{Err, Result, err};
use tuwunel_service::{Services, rendezvous::Meta};

pub(crate) use self::{
	create::create_route,
	delete::delete_route,
	get::get_route,
	msc4388::{
		create_route as create_msc4388_route, delete_route as delete_msc4388_route,
		discover_route as discover_msc4388_route, get_route as get_msc4388_route,
		put_route as put_msc4388_route,
	},
	put::put_route,
};

pub(super) const TEXT_PLAIN: &str = "text/plain";

pub(super) fn ensure_enabled(services: &Services) -> Result {
	services
		.config
		.rendezvous_enabled
		.then_some(())
		.ok_or_else(|| err!(Request(Unrecognized("QR login rendezvous is disabled"))))
}

#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn read_plain_body(
	headers: &HeaderMap,
	body: Body,
	max_bytes: usize,
) -> Result<Bytes> {
	validate_content_type(headers)?;

	let oversized = headers
		.get(CONTENT_LENGTH)
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.parse::<usize>().ok())
		.is_some_and(|length| length > max_bytes);

	if oversized {
		return Err!(Request(TooLarge("Rendezvous payload is too large")));
	}

	to_bytes(body, max_bytes)
		.await
		.map_err(|e| err!(Request(TooLarge("Rendezvous payload is too large: {e}"))))
}

fn validate_content_type(headers: &HeaderMap) -> Result {
	let content_type = headers
		.get(CONTENT_TYPE)
		.ok_or_else(|| err!(Request(MissingParam("Missing required header: content-type"))))?
		.to_str()
		.map_err(|_| err!(Request(InvalidParam("Content-Type must be text/plain"))))?;

	content_type
		.split(';')
		.next()
		.is_some_and(|essence| essence.trim().eq_ignore_ascii_case(TEXT_PLAIN))
		.then_some(())
		.ok_or_else(|| err!(Request(InvalidParam("Content-Type must be text/plain"))))
}

pub(super) fn session_response(
	status: StatusCode,
	body: Body,
	content_type: Option<&'static str>,
	meta: &Meta,
) -> Result<Response> {
	let mut response = Response::builder()
		.status(status)
		.body(body)
		.expect("rendezvous response builds");

	if let Some(content_type) = content_type {
		response
			.headers_mut()
			.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
	}

	session_headers(response.headers_mut(), meta)?;

	Ok(response)
}

pub(super) fn session_headers(headers: &mut HeaderMap, meta: &Meta) -> Result {
	let etag = HeaderValue::from_bytes(meta.etag.as_bytes()).expect("valid rendezvous ETag");
	let expires = system_time_to_http_date(&meta.expires_at)
		.map_err(|e| err!("Invalid rendezvous expiry: {e}"))?;
	let last_modified = system_time_to_http_date(&meta.last_modified)
		.map_err(|e| err!("Invalid rendezvous modification time: {e}"))?;

	headers.insert(ETAG, etag);
	headers.insert(EXPIRES, expires);
	headers.insert(LAST_MODIFIED, last_modified);
	headers.insert(PRAGMA, HeaderValue::from_static("no-cache"));
	headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store, no-transform"));

	Ok(())
}
