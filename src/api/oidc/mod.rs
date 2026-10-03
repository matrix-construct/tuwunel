pub(super) mod account;
pub(super) mod auth_issuer;
pub(super) mod auth_metadata;
pub(super) mod authorize;
pub(super) mod complete;
pub(super) mod device;
pub(super) mod jwks;
pub(super) mod native;
pub(super) mod registration;
pub(super) mod revoke;
pub(super) mod token;
pub(super) mod userinfo;

#[cfg(test)]
mod tests;

use axum::{Json, body::Body, response::IntoResponse};
use http::{Response, StatusCode};
use ruma::{OwnedUserId, UserId};
use serde_json::json;
pub(crate) use tuwunel_core::utils::url::url_encode;
use tuwunel_core::{Result, err};
use tuwunel_service::Services;
use url::Url;

pub(super) use self::{
	account::*, auth_issuer::*, auth_metadata::*, authorize::*, complete::*, device::*, jwks::*,
	native::*, registration::*, revoke::*, token::*, userinfo::*,
};

const OIDC_REQ_ID_LENGTH: usize = 32;

#[derive(Clone, Copy)]
struct NativeChoice {
	native_enabled: bool,
	has_default_idp: bool,
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response<Body> {
	let body = json!({
		"error": error,
		"error_description": description,
	});

	(status, Json(body)).into_response()
}

#[tracing::instrument(level = "debug", skip_all)]
async fn require_account_usable(services: &Services, user_id: &UserId) -> Result {
	services.users.deactivated_check(user_id).await?;
	services.users.locked_check(user_id).await
}

async fn consume_login_token(services: &Services, token: Option<&str>) -> Result<OwnedUserId> {
	let token = token.ok_or_else(|| err!(Request(Forbidden("Missing login token"))))?;

	services
		.users
		.find_from_login_token(token)
		.await
		.map_err(|_| err!(Request(Forbidden("Invalid or expired login token"))))
}

/// Verify a login token without consuming it; it is consumed later when the
/// confirmation form is submitted.
async fn peek_login_token(services: &Services, token: Option<&str>) -> Result<OwnedUserId> {
	let token = token.ok_or_else(|| err!(Request(Forbidden("Missing login token"))))?;

	services
		.users
		.peek_login_token(token)
		.await
		.map_err(|_| err!(Request(Forbidden("Invalid or expired login token"))))
}

/// Whether a redirect URI is covered by the operator's redirect allowlist.
///
/// A URI carrying a host matches an allowlist entry naming that host. A
/// private-use scheme carries no host at all (RFC 8252 §7.1, as in
/// `io.element.android:/callback`), so it matches an entry naming the scheme
/// instead, which is what lets one list cover both a web and a mobile client.
/// Either way the comparison ignores case.
fn redirect_allowlisted(allowed: &[String], uri: &str) -> bool {
	Url::parse(uri).is_ok_and(|url| {
		let name = url.host_str().unwrap_or_else(|| url.scheme());

		allowed
			.iter()
			.any(|entry| entry.eq_ignore_ascii_case(name))
	})
}

/// Whether a flow with no provider chooser serves the native page.
///
/// Native applies only when native auth is enabled and no default provider is
/// configured; every other flow goes through single sign-on.
fn should_serve_native(NativeChoice { native_enabled, has_default_idp }: NativeChoice) -> bool {
	native_enabled && !has_default_idp
}

/// Build the upstream SSO redirect URL for a pending authorization request.
///
/// The provider hands the browser back to the completion route carrying the
/// request id, where the authorization code is minted. A trailing slash on the
/// issuer is ignored.
fn authorization_sso_url(issuer: &str, idp_id: &str, req_id: &str) -> Result<Url> {
	let base = issuer.trim_end_matches('/');
	let complete = format!("{base}/_tuwunel/oidc/_complete");
	let callback = Url::parse_with_params(&complete, [("oidc_req_id", req_id)])
		.map_err(|_| err!(error!("Failed to build complete URL")))?;

	sso_redirect_url(base, idp_id, &callback)
}

fn sso_redirect_url(base: &str, idp_id: &str, callback: &Url) -> Result<Url> {
	let idp_id_enc = url_encode(idp_id);
	let mut sso_url =
		Url::parse(&format!("{base}/_matrix/client/v3/login/sso/redirect/{idp_id_enc}"))
			.map_err(|_| err!(error!("Failed to build SSO URL")))?;

	sso_url
		.query_pairs_mut()
		.append_pair("redirectUrl", callback.as_str());

	Ok(sso_url)
}
