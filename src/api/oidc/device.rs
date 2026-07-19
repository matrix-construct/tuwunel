mod consent;
mod entry;
mod error;
mod result;

use axum::{
	Json,
	extract::{Form, Request, State},
	response::{Html, IntoResponse, Redirect, Response},
};
use http::{
	StatusCode,
	header::{CACHE_CONTROL, CONTENT_SECURITY_POLICY, PRAGMA, REFERRER_POLICY},
};
use serde::Deserialize;
use serde_json::json;
use tuwunel_core::{Err, Error, Result, err};
use tuwunel_service::{
	Services,
	oauth::server::{DEVICE_GRANT_INTERVAL_SECS, DEVICE_GRANT_LIFETIME, format_user_code},
};
use url::Url;

use self::{consent::consent_html, entry::entry_html, error::error_html, result::result_html};
use super::{
	authorize::should_serve_native, consume_login_token, oauth_error, peek_login_token,
	sso_redirect_url, url_encode,
};
use crate::ClientIp;

// Per-response CSP: the consent form needs form-action 'self', which the global
// policy forbids.
static DEVICE_CSP: &str = "default-src 'none'; style-src 'self'; form-action 'self'; \
                           frame-ancestors 'none'; base-uri 'none';";

static DEVICE_HEAD: &str = r#"
	<meta charset="UTF-8">
	<link rel="stylesheet" href="/_tuwunel/oidc/account.css">
"#;

#[derive(Debug, Deserialize)]
pub(crate) struct DeviceAuthRequest {
	client_id: Option<String>,
	scope: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct DeviceVerifyParams {
	user_code: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct DeviceCallbackParams {
	user_code: Option<String>,

	#[serde(rename = "loginToken")]
	login_token: Option<String>,

	action: Option<String>,
}

/// RFC 8628 §3.1: the device authorization endpoint. Mints a `device_code` /
/// `user_code` pair and returns the verification URIs for the user.
pub(crate) async fn device_authorization_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	Form(body): Form<DeviceAuthRequest>,
) -> impl IntoResponse {
	let inner = if services
		.oauth
		.check_device_rate_limit(client)
		.is_err()
	{
		oauth_error(StatusCode::TOO_MANY_REQUESTS, "slow_down", "Too many requests")
	} else {
		device_authorization(&services, &body)
			.await
			.unwrap_or_else(device_authorization_error)
	};

	([(CACHE_CONTROL, "no-store"), (PRAGMA, "no-cache")], inner).into_response()
}

async fn device_authorization(services: &Services, body: &DeviceAuthRequest) -> Result<Response> {
	let client_id = body
		.client_id
		.as_deref()
		.ok_or_else(|| err!(Request(InvalidParam("client_id is required"))))?;

	let server = services.oauth.get_server()?;
	if server.get_client(client_id).await.is_err() {
		return Ok(oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", "Unknown client_id"));
	}

	let scope = body.scope.as_deref().unwrap_or_default();
	let grant = server.create_device_grant(client_id, scope);
	let user_code = format_user_code(&grant.user_code);

	let issuer = server.issuer_url()?;
	let base = issuer.trim_end_matches('/');
	let verification_uri = format!("{base}/_tuwunel/oidc/device");
	let verification_uri_complete =
		format!("{verification_uri}?user_code={}", url_encode(&user_code));

	let response = json!({
		"device_code": grant.device_code,
		"user_code": user_code,
		"verification_uri": verification_uri,
		"verification_uri_complete": verification_uri_complete,
		"expires_in": DEVICE_GRANT_LIFETIME.as_secs(),
		"interval": DEVICE_GRANT_INTERVAL_SECS,
	});

	Ok(Json(response).into_response())
}

#[expect(clippy::needless_pass_by_value)]
fn device_authorization_error(e: Error) -> Response {
	if !e.status_code().is_client_error() {
		return oauth_error(
			StatusCode::INTERNAL_SERVER_ERROR,
			"server_error",
			"An internal error occurred",
		);
	}

	oauth_error(StatusCode::BAD_REQUEST, "invalid_request", &e.sanitized_message())
}

/// RFC 8628 §3.3: the `verification_uri`. Shows a user-code entry form, or
/// sends the user through native or SSO authentication before consent.
pub(crate) async fn get_device_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	request: Request,
) -> impl IntoResponse {
	if services
		.oauth
		.check_device_rate_limit(client)
		.is_err()
	{
		return device_html_response(
			StatusCode::TOO_MANY_REQUESTS,
			entry_html(Some("Too many requests. Please wait and try again.")),
		);
	}

	let params: DeviceVerifyParams =
		match serde_html_form::from_str(request.uri().query().unwrap_or_default()) {
			| Err(e) => return device_error_response(&e.into()),
			| Ok(params) => params,
		};

	match handle_device_verify(&services, params.user_code.as_deref()) {
		| Ok(response) => response,
		| Err(e) => device_error_response(&e),
	}
}

fn handle_device_verify(services: &Services, user_code: Option<&str>) -> Result<Response> {
	let Some(user_code) = user_code.filter(|code| !code.is_empty()) else {
		return Ok(device_html_response(StatusCode::OK, entry_html(None)));
	};

	// Validating the code before authentication exposes the RFC 8628 §5.1
	// brute-force oracle, so defer it to the authenticated callback.
	let idp_id = services.oauth.providers.get_default_id();
	let serve_native =
		should_serve_native(services.config.oidc_native_auth, idp_id.is_some(), false);

	match serve_native {
		| true => device_native_redirect(services, user_code),
		| false => device_sso_redirect(services, user_code, idp_id.as_deref()),
	}
}

fn device_native_redirect(services: &Services, user_code: &str) -> Result<Response> {
	let issuer = services.oauth.get_server()?.issuer_url()?;
	let base = issuer.trim_end_matches('/');

	let native_url = Url::parse(&format!("{base}/_tuwunel/oidc/native"))
		.map(|mut url| {
			url.query_pairs_mut()
				.append_pair("user_code", user_code);
			url
		})
		.map_err(|_| err!(Request(InvalidParam("Failed to build native login URL"))))?;

	Ok(device_redirect_response(Redirect::temporary(native_url.as_str())))
}

fn device_sso_redirect(
	services: &Services,
	user_code: &str,
	idp_id: Option<&str>,
) -> Result<Response> {
	let idp_id = idp_id
		.ok_or_else(|| err!(Config("identity_provider", "No identity provider configured")))?;

	let issuer = services.oauth.get_server()?.issuer_url()?;
	let base = issuer.trim_end_matches('/');

	let mut callback_url = Url::parse(&format!("{base}/_tuwunel/oidc/device_callback"))
		.map_err(|_| err!(Request(InvalidParam("Failed to build device callback URL"))))?;

	callback_url
		.query_pairs_mut()
		.append_pair("user_code", user_code);

	let sso_url = sso_redirect_url(base, idp_id, &callback_url)?;

	Ok(device_redirect_response(Redirect::temporary(sso_url.as_str())))
}

/// The authentication return target: renders consent for the authenticated
/// user.
pub(crate) async fn get_device_callback_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	request: Request,
) -> impl IntoResponse {
	if services
		.oauth
		.check_device_rate_limit(client)
		.is_err()
	{
		return device_html_response(
			StatusCode::TOO_MANY_REQUESTS,
			error_html("Too many requests. Please wait and try again."),
		);
	}

	let params: DeviceCallbackParams =
		match serde_html_form::from_str(request.uri().query().unwrap_or_default()) {
			| Err(e) => return device_error_response(&e.into()),
			| Ok(params) => params,
		};

	match handle_device_callback_get(&services, params).await {
		| Ok(html) => device_html_response(StatusCode::OK, html),
		| Err(e) => device_error_response(&e),
	}
}

async fn handle_device_callback_get(
	services: &Services,
	params: DeviceCallbackParams,
) -> Result<String> {
	let token = params.login_token.as_deref();
	let user_id = peek_login_token(services, token).await?;

	let user_code = params.user_code.as_deref().unwrap_or_default();
	let server = services.oauth.get_server()?;

	// A failed guess burns the login token (RFC 8628 §5.1; see
	// verify_device_grant).
	let grant = match server.verify_device_grant(user_code).await {
		| Ok(grant) => grant,
		| Err(e) => {
			consume_login_token(services, token).await.ok();

			return Err(e);
		},
	};

	let client_name = server
		.get_client(&grant.client_id)
		.await
		.ok()
		.and_then(|client| client.client_name);

	let client_label = client_name.as_deref().unwrap_or(&grant.client_id);

	Ok(consent_html(
		&user_id,
		client_label,
		&grant.user_code,
		&grant.scope,
		token.unwrap_or_default(),
	))
}

pub(crate) async fn post_device_callback_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	Form(body): Form<DeviceCallbackParams>,
) -> impl IntoResponse {
	if services
		.oauth
		.check_device_rate_limit(client)
		.is_err()
	{
		return device_html_response(
			StatusCode::TOO_MANY_REQUESTS,
			error_html("Too many requests. Please wait and try again."),
		);
	}

	match handle_device_callback_post(&services, body).await {
		| Ok(html) => device_html_response(StatusCode::OK, html),
		| Err(e) => device_error_response(&e),
	}
}

async fn handle_device_callback_post(
	services: &Services,
	body: DeviceCallbackParams,
) -> Result<String> {
	let user_code = body.user_code.as_deref().unwrap_or_default();
	let action = body.action.as_deref().unwrap_or_default();
	let user_id = consume_login_token(services, body.login_token.as_deref()).await?;
	let server = services.oauth.get_server()?;

	match action {
		| "approve" => {
			let idp_id = services.oauth.providers.get_default_id();
			server
				.approve_device_grant(user_code, user_id, idp_id)
				.await?;

			Ok(result_html(
				"Device approved",
				"You have signed in. Return to your device; it will continue automatically.",
			))
		},

		| "deny" => {
			server.deny_device_grant(user_code).await?;

			Ok(result_html(
				"Sign-in denied",
				"The sign-in request was denied. You can close this page.",
			))
		},

		| _ => Err!(Request(InvalidParam("Unknown action"))),
	}
}

fn device_redirect_response(redirect: Redirect) -> Response {
	([(CACHE_CONTROL, "no-store"), (REFERRER_POLICY, "no-referrer")], redirect).into_response()
}

fn device_html_response(status: StatusCode, html: String) -> Response {
	let headers = [
		(CACHE_CONTROL, "no-store"),
		(CONTENT_SECURITY_POLICY, DEVICE_CSP),
		(REFERRER_POLICY, "no-referrer"),
	];

	(status, headers, Html(html)).into_response()
}

fn device_error_response(error: &Error) -> Response {
	device_html_response(error.status_code(), error_html(&error.sanitized_message()))
}
