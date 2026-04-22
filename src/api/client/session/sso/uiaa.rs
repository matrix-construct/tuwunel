use axum::extract::State;
use ruma::api::client::uiaa::{AuthType, UiaaInfo, get_uiaa_fallback_page};
use serde_json::Value as JsonValue;
use tuwunel_core::{Err, Result, trace, utils::BoolExt};

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
	let session_data = services
		.uiaa
		.get_uiaa_session_by_session_id(session)
		.await
		.inspect(|session_data| trace!(?session_data));

	if session_data
		.as_ref()
		.is_some_and(|(_, _, uiaainfo)| completed(uiaainfo))
	{
		let html = include_str!("complete.html");

		return Ok(Response::html(html.as_bytes().to_vec()));
	}

	// Check if this UIAA session has any flow with an SSO stage.
	let has_flow_with_sso_stage = || {
		session_data
			.as_ref()
			.is_some_and(|(_, _, uiaainfo)| {
				uiaainfo
					.flows
					.iter()
					.any(|flow| flow.stages.contains(&AuthType::Sso))
			})
	};

	// Session is not completed yet. Read the IdP that was bound to this UIAA
	// session at creation time from the stored UiaaInfo params. The IdP must
	// always be present — auth_uiaa only advertises m.login.sso when it can
	// determine exactly one provider, so a missing IdP here is a logic error.
	let idp_id: Option<String> = session_data
		.as_ref()
		.map(|(_, _, uiaainfo)| uiaainfo)
		.inspect(|uiaainfo| trace!(?uiaainfo))
		.and_then(|uiaainfo| {
			let raw = uiaainfo.params.as_ref()?.get();
			let params: JsonValue = serde_json::from_str(raw).ok()?;

			params["m.login.sso"]["identity_providers"]
				.as_array()?
				.first()?["id"]
				.as_str()
				.map(ToOwned::to_owned)
		})
		.or_else(|| {
			has_flow_with_sso_stage()
				.is_false()
				.then_some(String::new())
		});

	// The IdP MUST have been bound at UIAA session creation time.
	// If it is missing, auth_uiaa should not have advertised m.login.sso.
	// Returning an error is safer than routing to an arbitrary provider.
	let Some(ref idp) = idp_id else {
		return Err!(Request(Forbidden(
			"No SSO provider bound to this UIAA session; cannot complete re-authentication"
		)));
	};

	let empty_or_slash = idp
		.is_empty()
		.then_some(idp.as_str())
		.unwrap_or("/");

	let url_str = format!(
		"/_matrix/client/v3/login/sso/redirect{}{}?redirectUrl=uiaa:{}",
		empty_or_slash,
		url_encode(idp),
		url_encode(session)
	);

	let html = include_str!("required.html");
	let output = html.replace("{{url_str}}", &url_str);

	Ok(Response::html(output.into_bytes()))
}
