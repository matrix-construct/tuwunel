use std::time::Duration;

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
use ruma::{OwnedDeviceId, UserId};
use serde::Deserialize;
use serde_json::json;
use tuwunel_core::{
	Err, Error, Result, err, info,
	utils::{
		BoolExt,
		future::OptionFutureExt,
		time::{now, timepoint_has_passed},
	},
	warn,
};
use tuwunel_service::{
	Services,
	oauth::server::{DeviceGrantPoll, IdTokenClaims, Server, narrow_scope},
	users::device::{RefreshToken, generate_refresh_token},
};

use super::oauth_error;
use crate::ClientIp;

#[derive(Debug, Deserialize)]
pub(crate) struct TokenRequest {
	grant_type: String,
	code: Option<String>,
	redirect_uri: Option<String>,
	client_id: Option<String>,
	code_verifier: Option<String>,
	refresh_token: Option<String>,
	device_code: Option<String>,
	#[serde(rename = "scope")]
	_scope: Option<String>,
}

/// An authenticated grant ready to mint tokens.
struct ApprovedGrant<'a> {
	client_id: &'a str,
	scope: &'a str,
	user_id: &'a UserId,
	nonce: Option<String>,
	idp_id: Option<String>,
}

pub(crate) async fn token_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	Form(body): Form<TokenRequest>,
) -> impl IntoResponse {
	// RFC 6749 §5.1 and §5.2 require Cache-Control: no-store and Pragma: no-cache
	// on all token endpoint responses (success and error).
	let inner = if services.oauth.check_rate_limit(client).is_err() {
		oauth_error(StatusCode::TOO_MANY_REQUESTS, "slow_down", "Too many token requests")
	} else {
		match body.grant_type.as_str() {
			| "authorization_code" => token_authorization_code(&services, &body)
				.await
				.unwrap_or_else(token_error_response),

			| "refresh_token" => token_refresh(&services, &body)
				.await
				.unwrap_or_else(token_error_response),

			| "urn:ietf:params:oauth:grant-type:device_code" =>
				token_device_code(&services, &body)
					.await
					.unwrap_or_else(token_error_response),

			| _ => oauth_error(
				StatusCode::BAD_REQUEST,
				"unsupported_grant_type",
				"Unsupported grant_type",
			),
		}
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
		.exchange_auth_code(
			code,
			client_id,
			redirect_uri,
			body.code_verifier.as_deref(),
			services.server.config.oidc_require_pkce,
		)
		.await?;

	issue_tokens(services, ApprovedGrant {
		client_id: &session.client_id,
		scope: &session.scope,
		user_id: &session.user_id,
		nonce: session.nonce,
		idp_id: session.idp_id,
	})
	.await
}

/// RFC 8628 §3.4: poll the device-code grant; pending, denied and expired map
/// to the §3.5 error codes.
async fn token_device_code(services: &Services, body: &TokenRequest) -> Result<Response<Body>> {
	let device_code = body
		.device_code
		.as_deref()
		.ok_or_else(|| err!(Request(InvalidParam("device_code is required"))))?;

	let client_id = body
		.client_id
		.as_deref()
		.ok_or_else(|| err!(Request(InvalidParam("client_id is required"))))?;

	match services
		.oauth
		.get_server()?
		.poll_device_grant(device_code, client_id)
		.await?
	{
		| DeviceGrantPoll::Pending => Ok(oauth_error(
			StatusCode::BAD_REQUEST,
			"authorization_pending",
			"The user has not yet completed authorization",
		)),

		| DeviceGrantPoll::Denied => Ok(oauth_error(
			StatusCode::BAD_REQUEST,
			"access_denied",
			"The authorization request was denied",
		)),

		| DeviceGrantPoll::Expired => Ok(oauth_error(
			StatusCode::BAD_REQUEST,
			"expired_token",
			"The device code has expired",
		)),

		| DeviceGrantPoll::Approved(grant) =>
			issue_tokens(services, ApprovedGrant {
				client_id: &grant.client_id,
				scope: &grant.scope,
				user_id: &grant.user_id,
				nonce: None,
				idp_id: grant.idp_id,
			})
			.await,
	}
}

/// Mint the access token, refresh token and device for an authenticated grant,
/// honoring the MSC2967 device scope and emitting the OAuth token response.
async fn issue_tokens(services: &Services, grant: ApprovedGrant<'_>) -> Result<Response<Body>> {
	let ApprovedGrant { client_id, scope, user_id, nonce, idp_id } = grant;

	let (granted_scope, requested_device_id) =
		narrow_scope(scope, services.server.config.oidc_strict_scope)?;

	let requested_device: Option<OwnedDeviceId> = requested_device_id
		.as_deref()
		.map(OwnedDeviceId::from);

	if requested_device.is_none() && services.server.config.oidc_require_device_scope {
		return Err!(Request(InvalidParam(
			"a device scope (urn:matrix:client:device:<id>) is required"
		)));
	}

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

	let iss = services.oauth.get_server()?.issuer_url()?;
	let id_token = granted_scope
		.contains("openid")
		.then(|| {
			let now = now().as_secs();
			let claims = IdTokenClaims {
				iss,
				sub: user_id.to_string(),
				aud: client_id.to_owned(),
				exp: now.saturating_add(3600),
				iat: now,
				nonce,
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
			requested_device.as_deref(),
			(Some(&access_token), expires_in),
			Some(&refresh_token),
			Some(device_display_name),
			None,
		)
		.await?;

	// Tag the device with the IdP that authenticated it; a native (local
	// account) grant carries no provider, so the device stays untagged.
	if let Some(idp_id) = idp_id.filter(|idp| !idp.is_empty()) {
		services
			.users
			.mark_oidc_device(user_id, &device_id, &idp_id);
	}

	info!("{user_id} logged in via OIDC on {device_id} ({device_display_name})");

	// MSC2967: echo a server-chosen device id back in the scope when the client
	// omitted one.
	let scope = if requested_device.is_some() {
		granted_scope
	} else {
		warn!(%user_id, %device_id, "OIDC client omitted the device scope; generated a device id");

		let sep = if granted_scope.is_empty() { "" } else { " " };
		format!("{granted_scope}{sep}urn:matrix:client:device:{device_id}")
	};

	let mut response = json!({
		"access_token": access_token,
		"refresh_token": refresh_token,
		"scope": scope,
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
	let presented = body
		.refresh_token
		.as_deref()
		.ok_or_else(|| err!(Request(InvalidParam("refresh_token is required"))))?;

	match services
		.users
		.classify_refresh_token(presented)
		.await
	{
		| RefreshToken::Current { user_id, device_id, expires_at } => {
			if expires_at.is_some_and(timepoint_has_passed) {
				services
					.server
					.config
					.refresh_token_hard_logout
					.then_async(|| services.users.remove_device(&user_id, &device_id))
					.unwrap_or_else_async(async || {
						services
							.users
							.remove_refresh_token(&user_id, &device_id)
							.await
							.ok();
					})
					.await;

				return Err!(Request(Forbidden("Refresh token has expired")));
			}

			let (access_token, expires_in) = services.users.generate_access_token(true);
			let refresh_token = generate_refresh_token();
			services
				.users
				.set_access_token(
					&user_id,
					&device_id,
					&access_token,
					expires_in,
					Some(&refresh_token),
				)
				.await?;

			token_refresh_response(&access_token, &refresh_token, expires_in)
		},

		| RefreshToken::Replayed { user_id, device_id, current, grace } if grace => {
			// Benign double-submit: re-issue an access token for the unchanged
			// refresh token rather than rotating it.
			let (access_token, expires_in) = services.users.generate_access_token(true);
			services
				.users
				.set_access_token(&user_id, &device_id, &access_token, expires_in, None)
				.await?;

			token_refresh_response(&access_token, &current, expires_in)
		},

		| RefreshToken::Replayed { user_id, device_id, .. } => {
			let revoke = services.server.config.refresh_token_reuse_revoke;
			warn!(%user_id, %device_id, revoke, "OIDC refresh token reused after rotation");

			if revoke {
				services
					.users
					.remove_device(&user_id, &device_id)
					.await;
			}

			Err!(Request(Forbidden("Refresh token has already been used")))
		},

		| RefreshToken::Unknown => Err!(Request(Forbidden("Invalid refresh token"))),
	}
}

fn token_refresh_response(
	access_token: &str,
	refresh_token: &str,
	expires_in: Option<Duration>,
) -> Result<Response<Body>> {
	let mut response = json!({
		"access_token": access_token,
		"refresh_token": refresh_token,
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
