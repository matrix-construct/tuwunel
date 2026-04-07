use axum::{Json, extract::State, response::IntoResponse};
use serde::Serialize;
use tuwunel_core::Result;

#[derive(Serialize)]
struct AuthIssuerResponse {
	issuer: String,
}

pub(crate) async fn auth_issuer_route(
	State(services): State<crate::State>,
) -> Result<impl IntoResponse> {
	let issuer = services.oauth.get_server()?.issuer_url()?;

	Ok(Json(AuthIssuerResponse { issuer }))
}
