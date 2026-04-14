use axum::extract::State;
use ruma::api::client::uiaa::{AuthType, UiaaInfo, get_uiaa_fallback_page};
use tuwunel_core::{Err, Result};

use crate::{Ruma, oidc::url_encode};

/// # `GET /_matrix/client/v3/auth/m.login.sso/fallback/web?session={session_id}`
///
/// Get UIAA fallback web page for SSO authentication.
#[tracing::instrument(
	name = "sso_fallback",
	level = "debug",
	skip_all,
	fields(session = body.body.session),
)]
pub(crate) async fn sso_fallback_route(
	State(services): State<crate::State>,
	body: Ruma<get_uiaa_fallback_page::v3::Request>,
) -> Result<get_uiaa_fallback_page::v3::Response> {
	use get_uiaa_fallback_page::v3::Response;

	let session = &body.body.session;

	// Check if this UIAA session has already been completed via SSO or OAuth
	let completed = |uiaainfo: &UiaaInfo| {
		uiaainfo.completed.contains(&AuthType::Sso)
			|| uiaainfo.completed.contains(&AuthType::OAuth)
	};

	// Single DB lookup — get_uiaa_session_by_session_id does a full table scan,
	// so we call it once and reuse the result for both the completion check and
	// the IdP extraction that follows.
	let session_data = services.uiaa.get_uiaa_session_by_session_id(session).await;

	if session_data.as_ref().is_some_and(|(_, _, uiaainfo)| completed(uiaainfo)) {
		let html = include_str!("complete.html");

		return Ok(Response::html(html.as_bytes().to_vec()));
	}

	// Session is not completed yet. Read the IdP that was bound to this UIAA
	// session at creation time from the stored UiaaInfo params. The IdP must
	// always be present — auth_uiaa only advertises m.login.sso when it can
	// determine exactly one provider, so a missing IdP here is a logic error.
	let idp_id: Option<String> = session_data.and_then(|(_, _, uiaainfo)| {
		let raw = uiaainfo.params?;
		let params: serde_json::Value = serde_json::from_str(raw.get()).ok()?;
		params["m.login.sso"]["identity_providers"]
			.as_array()?
			.first()?["id"]
			.as_str()
			.map(ToOwned::to_owned)
	});

	// The IdP MUST have been bound at UIAA session creation time.
	// If it is missing, auth_uiaa should not have advertised m.login.sso.
	// Returning an error is safer than routing to an arbitrary provider.
	let Some(ref idp) = idp_id else {
		return Err!(Request(Forbidden(
			"No SSO provider bound to this UIAA session; cannot complete re-authentication"
		)));
	};

	let url_str = format!(
		"/_matrix/client/v3/login/sso/redirect/{}?redirectUrl=uiaa:{}",
		url_encode(idp),
		url_encode(session)
	);

	let html = include_str!("required.html");
	let output = html.replace("{{url_str}}", &url_str);

	Ok(Response::html(output.into_bytes()))
}
