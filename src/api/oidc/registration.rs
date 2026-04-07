use axum::{Json, extract::State, response::IntoResponse};
use http::StatusCode;
use serde_json::json;
use tuwunel_core::{Err, Result, info};
use tuwunel_service::oauth::server::DcrRequest;

pub(crate) async fn registration_route(
	State(services): State<crate::State>,
	Json(body): Json<DcrRequest>,
) -> Result<impl IntoResponse> {
	let Ok(oidc) = services.oauth.get_server() else {
		return Err!(Request(NotFound("OIDC server not configured")));
	};

	if body.redirect_uris.is_empty() {
		return Err!(Request(InvalidParam("redirect_uris must not be empty")));
	}

	let reg = oidc.register_client(body)?;

	info!(
		"OIDC client registered: {} ({})",
		reg.client_id,
		reg.client_name.as_deref().unwrap_or("unnamed")
	);

	Ok((
		StatusCode::CREATED,
		Json(json!({
			"client_id": reg.client_id,
			"client_id_issued_at": reg.registered_at,
			"redirect_uris": reg.redirect_uris,
			"client_name": reg.client_name,
			"client_uri": reg.client_uri,
			"logo_uri": reg.logo_uri,
			"contacts": reg.contacts,
			"token_endpoint_auth_method": reg.token_endpoint_auth_method,
			"grant_types": reg.grant_types,
			"response_types": reg.response_types,
			"application_type": reg.application_type,
			"policy_uri": reg.policy_uri,
			"tos_uri": reg.tos_uri,
			"software_id": reg.software_id,
			"software_version": reg.software_version,
		})),
	))
}
