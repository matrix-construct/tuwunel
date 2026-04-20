use axum::{
	Json,
	body::Body,
	extract::{Form, State},
	response::IntoResponse,
};
use http::{
	Response, StatusCode,
	header::{CACHE_CONTROL, PRAGMA},
};
use ruma::OwnedDeviceId;
use serde::Deserialize;
use serde_json::json;
use tuwunel_core::{Error, Result, err, info, utils::time::now};
use tuwunel_service::{
	Services,
	oauth::server::{IdTokenClaims, Server, extract_device_id},
	users::device::generate_refresh_token,
};

use super::oauth_error;

#[derive(Debug, Deserialize)]
pub(crate) struct TokenRequest {
	grant_type: String,
	code: Option<String>,
	redirect_uri: Option<String>,
	client_id: Option<String>,
	code_verifier: Option<String>,
	refresh_token: Option<String>,
	#[serde(rename = "scope")]
	_scope: Option<String>,
}

pub(crate) async fn token_route(
	State(services): State<crate::State>,
	Form(body): Form<TokenRequest>,
) -> impl IntoResponse {
	// RFC 6749 §5.1 and §5.2 require Cache-Control: no-store and Pragma: no-cache
	// on all token endpoint responses (success and error).
	let inner = match body.grant_type.as_str() {
		| "authorization_code" => token_authorization_code(&services, &body)
			.await
			.unwrap_or_else(token_error_response),

		| "refresh_token" => token_refresh(&services, &body)
			.await
			.unwrap_or_else(token_error_response),

		| _ => oauth_error(
			StatusCode::BAD_REQUEST,
			"unsupported_grant_type",
			"Unsupported grant_type",
		),
	};
	let mut response = inner.into_response();
	let headers = response.headers_mut();
	headers.insert(CACHE_CONTROL, http::HeaderValue::from_static("no-store"));
	headers.insert(PRAGMA, http::HeaderValue::from_static("no-cache"));
	response
}

async fn token_authorization_code(
	services: &Services,
	body: &TokenRequest,
) -> Result<Response<Body>> {
	let code = body
		.code
		.as_deref()
		.ok_or_else(|| err!(Request(InvalidParam("code is required"))))?;

	let redirect_uri = body
		.redirect_uri
		.as_deref()
		.ok_or_else(|| err!(Request(InvalidParam("redirect_uri is required"))))?;

	let client_id = body
		.client_id
		.as_deref()
		.ok_or_else(|| err!(Request(InvalidParam("client_id is required"))))?;

	let session = services
		.oauth
		.get_server()?
		.exchange_auth_code(code, client_id, redirect_uri, body.code_verifier.as_deref())
		.await?;

	let user_id = &session.user_id;
	let (access_token, expires_in) = services.users.generate_access_token(true);
	let refresh_token = generate_refresh_token();
	let client_name = services
		.oauth
		.get_server()?
		.get_client(client_id)
		.await
		.ok()
		.and_then(|c| c.client_name);

	let device_display_name = client_name.as_deref().unwrap_or("OIDC Client");
	let device_id: Option<OwnedDeviceId> =
		extract_device_id(&session.scope).map(OwnedDeviceId::from);

	let iss = services.oauth.get_server()?.issuer_url()?;
	let id_token = session
		.scope
		.contains("openid")
		.then(|| {
			let now = now().as_secs();
			let claims = IdTokenClaims {
				iss,
				sub: user_id.to_string(),
				aud: client_id.to_owned(),
				exp: now.saturating_add(3600),
				iat: now,
				nonce: session.nonce,
				at_hash: Some(Server::at_hash(&access_token)),
			};

			services
				.oauth
				.get_server()?
				.sign_id_token(&claims)
		})
		.transpose()?;

	let device_id = services
		.users
		.create_device(
			user_id,
			device_id.as_deref(),
			(Some(&access_token), expires_in),
			Some(&refresh_token),
			Some(device_display_name),
			None,
		)
		.await?;

	let idp_id = session.idp_id.as_deref().unwrap_or("");
	services
		.users
		.mark_oidc_device(user_id, &device_id, idp_id);

	info!("{user_id} logged in via OIDC on {device_id} ({device_display_name})");

	let mut response = json!({
		"access_token": access_token,
		"refresh_token": refresh_token,
		"scope": session.scope,
		"token_type": "Bearer",
	});

	if let Some(id_token) = id_token {
		response["id_token"] = json!(id_token);
	}

	if let Some(expires_in) = expires_in {
		response["expires_in"] = json!(expires_in.as_secs());
	}

	Ok(Json(response).into_response())
}

async fn token_refresh(services: &Services, body: &TokenRequest) -> Result<Response<Body>> {
	let refresh_token = body
		.refresh_token
		.as_deref()
		.ok_or_else(|| err!(Request(InvalidParam("refresh_token is required"))))?;

	let (user_id, device_id, _) = services
		.users
		.find_from_token(refresh_token)
		.await
		.map_err(|_| err!(Request(Forbidden("Invalid refresh token"))))?;

	let (new_access_token, expires_in) = services.users.generate_access_token(true);
	let new_refresh_token = generate_refresh_token();

	services
		.users
		.set_access_token(
			&user_id,
			&device_id,
			&new_access_token,
			expires_in,
			Some(&new_refresh_token),
		)
		.await?;

	let mut response = json!({
		"access_token": new_access_token,
		"refresh_token": new_refresh_token,
		"token_type": "Bearer",
	});

	if let Some(expires_in) = expires_in {
		response["expires_in"] = json!(expires_in.as_secs());
	}

	Ok(Json(response).into_response())
}

/// RFC 6749 §5.2: map error to correct HTTP status and OAuth2 error code.
/// Client-side errors (invalid grant, bad params) → 400 invalid_grant.
/// Server-side errors → 500 server_error with sanitized message.
#[expect(clippy::needless_pass_by_value)]
fn token_error_response(e: Error) -> Response<Body> {
	if !e.status_code().is_client_error() {
		return oauth_error(
			StatusCode::INTERNAL_SERVER_ERROR,
			"server_error",
			"An internal error occurred",
		);
	}

	oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", &e.sanitized_message())
}
