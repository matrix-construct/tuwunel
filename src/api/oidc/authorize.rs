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

use super::OIDC_REQ_ID_LENGTH;

#[derive(Debug, Deserialize)]
pub(crate) struct AuthorizeParams {
	client_id: String,
	redirect_uri: String,
	response_type: String,
	scope: String,
	state: Option<String>,
	nonce: Option<String>,
	code_challenge: Option<String>,
	code_challenge_method: Option<String>,
	#[serde(default, rename = "prompt")]
	_prompt: Option<String>,
}

pub(crate) async fn authorize_route(
	State(services): State<crate::State>,
	request: axum::extract::Request,
) -> Result<impl IntoResponse> {
	let oidc = services.oauth.get_server()?;

	let query = request.uri().query().unwrap_or_default();
	let params: AuthorizeParams = serde_html_form::from_str(query)?;

	if params.response_type != "code" {
		return Err!(Request(InvalidParam("Only response_type=code is supported")));
	}

	validate_redirect_uri(&services, &params).await?;

	let now = SystemTime::now();
	let req_id = utils::random_string(OIDC_REQ_ID_LENGTH);
	let idp_id = services
		.oauth
		.providers
		.get_default_id()
		.ok_or_else(|| err!(Config("identity_provider", "No identity provider configured")))?;

	let auth_req = AuthRequest {
		client_id: params.client_id,
		redirect_uri: params.redirect_uri,
		scope: params.scope,
		state: params.state,
		nonce: params.nonce,
		code_challenge: params.code_challenge,
		code_challenge_method: params.code_challenge_method,
		created_at: now,
		expires_at: now
			.checked_add(AUTH_REQUEST_LIFETIME)
			.unwrap_or(now),
	};

	let base = oidc.issuer_url()?;
	let base = base.trim_end_matches('/');

	let complete_url = Url::parse(&format!("{base}/_tuwunel/oidc/_complete"))
		.map_err(|_| err!(error!("Failed to build complete URL")))
		.map(|mut url| {
			url.query_pairs_mut()
				.append_pair("oidc_req_id", &req_id);
			url
		})?;

	let sso_url = Url::parse(&format!("{base}/_matrix/client/v3/login/sso/redirect/{idp_id}"))
		.map_err(|_| err!(error!("Failed to build SSO URL")))
		.map(|mut url| {
			url.query_pairs_mut()
				.append_pair("redirectUrl", complete_url.as_str());
			url
		})?;

	oidc.store_auth_request(&req_id, &auth_req);

	Ok(Redirect::temporary(sso_url.as_str()))
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
