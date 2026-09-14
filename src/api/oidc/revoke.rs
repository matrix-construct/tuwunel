use axum::{
	body::Body,
	extract::{Form, State},
	response::{IntoResponse, Response},
};
use http::{
	HeaderValue, StatusCode,
	header::{CACHE_CONTROL, PRAGMA},
};
use serde::Deserialize;
use tuwunel_service::Services;

use super::oauth_error;

/// MSC4254 / RFC7009 token revocation request body
/// (`application/x-www-form-urlencoded`).
///
/// `client_id` is accepted but not validated: per MSC4254 the server SHOULD
/// revoke even when `client_id` is missing or does not match, since
/// secret-scanning tools rely on this to neutralise leaked tokens.
#[derive(Debug, Deserialize)]
pub(crate) struct RevokeRequest {
	token: Option<String>,

	#[serde(default)]
	token_type_hint: Option<String>,

	#[serde(default, rename = "client_id")]
	_client_id: Option<String>,
}

/// `POST /_tuwunel/oidc/revoke`
///
/// MSC4254: OAuth 2.0 Token Revocation per RFC7009. Revokes both the access
/// and refresh tokens associated with the supplied token.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn revoke_route(
	State(services): State<crate::State>,
	Form(body): Form<RevokeRequest>,
) -> impl IntoResponse {
	let response = revoke(&services, body)
		.await
		.unwrap_or_else(|err| err);

	with_cache_headers(response)
}

async fn revoke(services: &Services, body: RevokeRequest) -> Result<Response, Response> {
	let token = body
		.token
		.filter(|t| !t.is_empty())
		.ok_or_else(|| {
			oauth_error(StatusCode::BAD_REQUEST, "invalid_request", "token parameter is required")
		})?;

	if let Some(hint) = body.token_type_hint.as_deref()
		&& !matches!(hint, "access_token" | "refresh_token")
	{
		return Err(oauth_error(
			StatusCode::BAD_REQUEST,
			"unsupported_token_type",
			"token_type_hint must be access_token or refresh_token",
		));
	}

	// RFC7009 §2.2: invalid or unknown tokens still produce a 200 OK.
	// remove_device drops both the access and refresh tokens (and the device).
	if let Ok((user_id, device_id, _)) = services.users.find_from_token(&token).await {
		services
			.users
			.remove_device(&user_id, &device_id)
			.await;
	}

	Ok(Response::builder()
		.status(StatusCode::OK)
		.body(Body::empty())
		.expect("empty 200 OK builds"))
}

fn with_cache_headers(mut response: Response) -> Response {
	let headers = response.headers_mut();
	headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
	headers.insert(PRAGMA, HeaderValue::from_static("no-cache"));
	response
}
