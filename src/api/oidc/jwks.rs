use axum::{Json, extract::State, response::IntoResponse};
use tuwunel_core::Result;

pub(crate) async fn jwks_route(
	State(services): State<crate::State>,
) -> Result<impl IntoResponse> {
	let oidc = services.oauth.get_server()?;

	Ok(Json(oidc.jwks()))
}
