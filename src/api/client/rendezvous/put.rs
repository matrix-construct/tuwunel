use axum::{
	Json,
	body::Body,
	extract::{Path, Request, State},
	response::{IntoResponse, Response},
};
use http::{StatusCode, header::IF_MATCH};
use serde::Serialize;
use tuwunel_core::{Err, Result, err};
use tuwunel_service::rendezvous::Put;

use super::{TEXT_PLAIN, ensure_enabled, read_plain_body, session_headers, session_response};

#[derive(Serialize)]
struct ConcurrentWriteResponse {
	errcode: &'static str,
	error: &'static str,

	#[serde(rename = "org.matrix.msc4108.errcode")]
	unstable_errcode: &'static str,
}

#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn put_rendezvous_route(
	State(services): State<crate::State>,
	Path(id): Path<String>,
	request: Request,
) -> Result<Response> {
	ensure_enabled(&services)?;

	let max_bytes = services.config.rendezvous_session_max_bytes;
	let (parts, body) = request.into_parts();
	let data = read_plain_body(&parts.headers, body, max_bytes).await?;
	let if_match = parts
		.headers
		.get(IF_MATCH)
		.ok_or_else(|| err!(Request(MissingParam("Missing required header: if-match"))))?
		.to_str()
		.map_err(|_| err!(Request(InvalidParam("If-Match must be a valid ETag"))))?;

	match services.rendezvous.put(&id, if_match, data) {
		| Put::NotFound => Err!(Request(NotFound("Rendezvous session not found"))),
		| Put::Accepted(meta) =>
			session_response(StatusCode::ACCEPTED, Body::empty(), Some(TEXT_PLAIN), &meta),
		| Put::PreconditionFailed(meta) => {
			let body = ConcurrentWriteResponse {
				errcode: "M_UNKNOWN",
				error: "ETag does not match",
				unstable_errcode: "M_CONCURRENT_WRITE",
			};
			let mut response = (StatusCode::PRECONDITION_FAILED, Json(body)).into_response();

			session_headers(response.headers_mut(), &meta)?;

			Ok(response)
		},
	}
}
