use axum::{
	Json,
	extract::{Request, State},
	response::{IntoResponse, Response},
};
use http::StatusCode;
use serde::Serialize;
use tuwunel_core::Result;

use super::{ensure_enabled, read_plain_body, session_headers};

#[derive(Serialize)]
struct CreateResponse {
	url: String,
}

#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn create_rendezvous_route(
	State(services): State<crate::State>,
	request: Request,
) -> Result<Response> {
	ensure_enabled(&services)?;

	let oidc = services.oauth.get_server()?;
	let base = oidc.issuer_url()?;
	let base = base.trim_end_matches('/');
	let max_bytes = services.config.rendezvous_session_max_bytes;
	let (parts, body) = request.into_parts();
	let data = read_plain_body(&parts.headers, body, max_bytes).await?;
	let (id, meta) = services.rendezvous.create(data);
	let url = format!("{base}/_matrix/client/unstable/org.matrix.msc4108/rendezvous/{id}");
	let mut response = (StatusCode::CREATED, Json(CreateResponse { url })).into_response();

	session_headers(response.headers_mut(), &meta)?;

	Ok(response)
}
