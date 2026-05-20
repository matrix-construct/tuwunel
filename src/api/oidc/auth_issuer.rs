use axum::{Json, extract::State, response::IntoResponse};
use http::StatusCode;
use ruma::api::client::error::ErrorKind;
use serde::Serialize;
use tuwunel_core::{Error, Result};

#[derive(Serialize)]
struct AuthIssuerResponse {
	issuer: String,
}

pub(crate) async fn auth_issuer_route(
	State(services): State<crate::State>,
) -> Result<impl IntoResponse> {
	// 404 + M_UNRECOGNIZED when OAuth is disabled (see auth_metadata.rs).
	let Ok(server) = services.oauth.get_server() else {
		return Err(Error::Request(
			ErrorKind::Unrecognized,
			"OIDC server not configured".into(),
			StatusCode::NOT_FOUND,
		));
	};
	let issuer = server.issuer_url()?;

	Ok(Json(AuthIssuerResponse { issuer }))
}
