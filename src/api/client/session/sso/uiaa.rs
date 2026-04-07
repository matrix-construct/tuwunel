use axum::extract::State;
use ruma::api::client::uiaa::{AuthType, UiaaInfo, get_uiaa_fallback_page};
use tuwunel_core::Result;

use crate::Ruma;

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

	if services
		.uiaa
		.get_uiaa_session_by_session_id(session)
		.await
		.as_ref()
		.is_some_and(|(_, _, uiaainfo)| completed(uiaainfo))
	{
		let html = include_str!("complete.html");

		return Ok(Response::html(html.as_bytes().to_vec()));
	}

	// Session is not completed yet, show the prompt to continue
	let html = include_str!("required.html");
	let url_str = format!("/_matrix/client/v3/login/sso/redirect?redirectUrl=uiaa:{session}");
	let output = html.replace("{{url_str}}", &url_str);

	Ok(Response::html(output.into_bytes()))
}
