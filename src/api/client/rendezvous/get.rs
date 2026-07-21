use axum::{
	body::Body,
	extract::{Path, State},
	response::Response,
};
use http::{HeaderMap, StatusCode, header::IF_NONE_MATCH};
use tuwunel_core::{Err, Result};
use tuwunel_service::rendezvous::Get;

use super::{TEXT_PLAIN, ensure_enabled, session_response};

#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn get_rendezvous_route(
	State(services): State<crate::State>,
	Path(id): Path<String>,
	headers: HeaderMap,
) -> Result<Response> {
	ensure_enabled(&services)?;

	let if_none_match = headers
		.get(IF_NONE_MATCH)
		.and_then(|value| value.to_str().ok());

	match services.rendezvous.get(&id, if_none_match) {
		| Get::NotFound => Err!(Request(NotFound("Rendezvous session not found"))),
		| Get::NotModified(meta) =>
			session_response(StatusCode::NOT_MODIFIED, Body::empty(), None, &meta),
		| Get::Data { data, meta } =>
			session_response(StatusCode::OK, Body::from(data), Some(TEXT_PLAIN), &meta),
	}
}
