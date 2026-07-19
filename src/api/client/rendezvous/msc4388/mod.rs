mod create;
mod delete;
mod discover;
mod get;
mod put;
#[cfg(test)]
mod tests;

use std::{net::IpAddr, result::Result as StdResult, str};

use axum::{
	Json,
	response::{IntoResponse, Response},
};
use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use serde::Serialize;
use tuwunel_core::{Error as CoreError, Result as CoreResult, err};
use tuwunel_service::Services;

pub(crate) use self::{
	create::create_route, delete::delete_route, discover::discover_route, get::get_route,
	put::put_route,
};
use crate::Ruma;

pub(crate) type Result<T> = StdResult<T, Error>;

#[derive(Debug)]
pub(crate) enum Error {
	Core(Box<CoreError>),
	ConcurrentWrite,
}

#[derive(Serialize)]
struct ConcurrentWriteResponse {
	errcode: &'static str,
	error: &'static str,
}

const MAX_DATA_BYTES: usize = 4_096;
const SEC_FETCH_DEST: &str = "sec-fetch-dest";
const SEC_FETCH_MODE: &str = "sec-fetch-mode";
const SEC_FETCH_SITE: &str = "sec-fetch-site";
const SEC_FETCH_USER: &str = "sec-fetch-user";

impl From<CoreError> for Error {
	fn from(error: CoreError) -> Self { Self::Core(Box::new(error)) }
}

impl IntoResponse for Error {
	fn into_response(self) -> Response {
		match self {
			| Self::Core(error) => (*error).into_response(),
			| Self::ConcurrentWrite => {
				let body = ConcurrentWriteResponse {
					errcode: "IO_ELEMENT_MSC4388_CONCURRENT_WRITE",
					error: "sequence_token does not match",
				};

				(StatusCode::CONFLICT, Json(body)).into_response()
			},
		}
	}
}

pub(super) fn ensure_available(services: &Services, client: IpAddr) -> CoreResult {
	super::ensure_enabled(services)?;

	services.rendezvous.check_rate_limit(client)
}

pub(super) fn ensure_create_available<T>(services: &Services, body: &Ruma<T>) -> CoreResult {
	services.oauth.get_server()?;

	let authenticated = body.sender_user.is_some() || body.appservice_info.is_some();

	(!services.config.rendezvous_authenticated_only || authenticated)
		.then_some(())
		.ok_or_else(|| {
			err!(Request(Forbidden("Rendezvous session creation requires authentication")))
		})
}

pub(super) fn ensure_data_size(services: &Services, data: &str) -> CoreResult {
	let max_bytes = max_data_bytes(services.config.rendezvous_session_max_bytes);

	(data.len() <= max_bytes)
		.then_some(())
		.ok_or_else(|| err!(Request(TooLarge("Rendezvous payload is too large"))))
}

fn max_data_bytes(configured: usize) -> usize { configured.min(MAX_DATA_BYTES) }

pub(super) fn ensure_safe_get(headers: &HeaderMap) -> CoreResult {
	let destination = headers
		.get(SEC_FETCH_DEST)
		.is_some_and(|value| value.as_bytes().ne(b"empty"));

	let navigation = headers
		.get(SEC_FETCH_MODE)
		.is_some_and(|value| value.as_bytes().eq(b"navigate"));

	let user_activation = headers
		.get(SEC_FETCH_USER)
		.is_some_and(|value| value.as_bytes().eq(b"?1"));

	let direct_request = headers
		.get(SEC_FETCH_SITE)
		.is_some_and(|value| value.as_bytes().eq(b"none"));

	(!destination && !navigation && !user_activation && !direct_request)
		.then_some(())
		.ok_or_else(|| {
			err!(Request(Forbidden("Rendezvous payload is unavailable to browser navigation")))
		})
}

pub(super) fn data_to_string(data: &Bytes) -> CoreResult<String> {
	str::from_utf8(data)
		.map(ToOwned::to_owned)
		.map_err(|_| err!(Request(Unknown("Rendezvous payload is not valid UTF-8"))))
}
