use std::{net::IpAddr, time::SystemTime};

use axum::{
	extract::State,
	response::{IntoResponse, Redirect},
};
use serde::Deserialize;
use tuwunel_core::{
	Err, Result, err, utils,
	utils::{BoolExt, result::FlatOk},
};
use tuwunel_service::{
	Services,
	oauth::server::{AUTH_REQUEST_LIFETIME, AuthRequest},
};
use url::Url;

use super::{OIDC_REQ_ID_LENGTH, sso_redirect_url};
use crate::ClientIp;

#[derive(Debug, Deserialize)]
pub(crate) struct AuthorizeParams {
	client_id: String,
	redirect_uri: String,
	response_type: String,
	response_mode: Option<String>,
	scope: String,
	state: Option<String>,
	nonce: Option<String>,
	code_challenge: Option<String>,
	code_challenge_method: Option<String>,
	#[serde(default)]
	idp_id: Option<String>,
	#[serde(default)]
	prompt: Option<String>,
}

pub(crate) async fn authorize_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	request: axum::extract::Request,
) -> Result<impl IntoResponse> {
	let oidc = services.oauth.get_server()?;
	services.oauth.check_rate_limit(client)?;

	let query = request.uri().query().unwrap_or_default();
	let params: AuthorizeParams = serde_html_form::from_str(query)?;

	if params.response_type != "code" {
		return Err!(Request(InvalidParam("Only response_type=code is supported")));
	}

	let response_mode = params.response_mode.as_deref().unwrap_or("query");
	if !matches!(response_mode, "query" | "fragment") {
		return Err!(Request(InvalidParam(
			"Only response_mode=query or response_mode=fragment is supported"
		)));
	}

	// RFC 7636 / MSC2964: require an explicit S256 challenge; bare `plain` is
	// rejected.
	match (&params.code_challenge, params.code_challenge_method.as_deref()) {
		| (None, _) if services.config.oidc_require_pkce =>
			return Err!(Request(InvalidParam("code_challenge is required (PKCE with S256)"))),

		| (Some(_), method) if method != Some("S256") =>
			return Err!(Request(InvalidParam("Only code_challenge_method=S256 is supported"))),

		| _ => {},
	}

	validate_redirect_uri(&services, &params).await?;

	let now = SystemTime::now();
	let req_id = utils::random_string(OIDC_REQ_ID_LENGTH);
	let base = oidc.issuer_url()?;
	let base = base.trim_end_matches('/');

	let resolved_idp: Option<String> = match params.idp_id.as_deref() {
		| Some(requested) => services
			.oauth
			.providers
			.get_config(requested)
			.map(|provider| Some(provider.id().to_owned()))
			.map_err(|_| err!(Request(InvalidParam("Unrecognized identity provider"))))?,

		| None => services.oauth.providers.get_default_id(),
	};

	// Native page when native auth is on and no external provider applies, or the
	// client explicitly requested account creation (prompt=create).
	let serve_native = params.idp_id.is_none()
		&& should_serve_native(
			services.config.oidc_native_auth,
			resolved_idp.is_some(),
			params.prompt.as_deref() == Some("create"),
		);

	let idp_id = match (serve_native, resolved_idp) {
		| (true, _) => None,
		| (false, Some(idp_id)) => Some(idp_id),
		| (false, None) =>
			return Err!(Config("identity_provider", "No identity provider configured")),
	};

	let auth_req = AuthRequest {
		client_id: params.client_id,
		redirect_uri: params.redirect_uri,
		scope: params.scope,
		state: params.state,
		nonce: params.nonce,
		code_challenge: params.code_challenge,
		code_challenge_method: params.code_challenge_method,
		// The IdP that authenticated the user, tagged on the device at token
		// exchange; absent in native mode (the account is local).
		idp_id: idp_id.clone(),
		response_mode: params.response_mode,
		created_at: now,
		expires_at: now
			.checked_add(AUTH_REQUEST_LIFETIME)
			.unwrap_or(now),
	};

	oidc.store_auth_request(&req_id, &auth_req);

	let Some(idp_id) = idp_id else {
		let view = match params.prompt.as_deref() {
			| Some("create") => "register",
			| _ => "login",
		};

		let native_url = Url::parse(&format!("{base}/_tuwunel/oidc/native"))
			.map_err(|_| err!(error!("Failed to build native auth URL")))
			.map(|mut url| {
				url.query_pairs_mut()
					.append_pair("oidc_req_id", &req_id)
					.append_pair("view", view);

				url
			})?;

		return Ok(Redirect::temporary(native_url.as_str()));
	};

	let complete_url = Url::parse(&format!("{base}/_tuwunel/oidc/_complete"))
		.map_err(|_| err!(error!("Failed to build complete URL")))
		.map(|mut url| {
			url.query_pairs_mut()
				.append_pair("oidc_req_id", &req_id);

			url
		})?;

	let sso_url = sso_redirect_url(base, &idp_id, &complete_url)?;

	Ok(Redirect::temporary(sso_url.as_str()))
}

/// Decide whether an authorization request with no explicitly-selected provider
/// is served the native login/register page rather than an upstream-IdP SSO
/// redirect. Native applies when enabled and either no default IdP is
/// configured or the client asked to create an account.
fn should_serve_native(native_enabled: bool, has_default_idp: bool, wants_create: bool) -> bool {
	native_enabled && (!has_default_idp || wants_create)
}

async fn validate_redirect_uri(services: &Services, params: &AuthorizeParams) -> Result {
	services
		.oauth
		.get_server()
		.expect("OIDC already configured")
		.get_client(&params.client_id)
		.await?
		.redirect_uris
		.iter()
		.any(|uri| redirect_uri_matches(uri, &params.redirect_uri))
		.into_option()
		.ok_or_else(|| err!(Request(InvalidParam("redirect_uri not registered for this client"))))
}

fn redirect_uri_matches(registered: &str, requested: &str) -> bool {
	match (Url::parse(registered), Url::parse(requested)) {
		| (..) if registered == requested => true,
		| (Ok(reg), Ok(req)) if is_loopback_redirect(&reg) && is_loopback_redirect(&req) =>
			reg.scheme() == req.scheme()
				&& reg.host_str() == req.host_str()
				&& reg.path() == req.path()
				&& reg.query() == req.query()
				&& reg.fragment() == req.fragment(),

		| _ => false,
	}
}

fn is_loopback_redirect(uri: &Url) -> bool {
	let addr = || uri.host_str().map(str::parse::<IpAddr>).flat_ok();

	uri.scheme() == "http" && matches!(addr(), Some(ip) if ip.is_loopback())
}

#[cfg(test)]
mod tests {
	use super::should_serve_native;

	#[test]
	fn native_decision_truth_table() {
		// Native auth disabled: never native.
		assert!(!should_serve_native(false, false, false));
		assert!(!should_serve_native(false, true, true));

		// Native-only (no default provider): native.
		assert!(should_serve_native(true, false, false));

		// An external default is configured, ordinary login: SSO to the default.
		assert!(!should_serve_native(true, true, false));

		// An external default is configured, prompt=create: native registration.
		assert!(should_serve_native(true, true, true));
	}
}
