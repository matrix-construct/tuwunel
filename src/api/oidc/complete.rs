use axum::{
	extract::State,
	response::{IntoResponse, Redirect},
};
use serde::Deserialize;
use tuwunel_core::{Err, Result, err};
use url::Url;

#[derive(Debug, Deserialize)]
pub(crate) struct CompleteParams {
	oidc_req_id: String,
	#[serde(rename = "loginToken")]
	login_token: String,
}

pub(crate) async fn complete_route(
	State(services): State<crate::State>,
	request: axum::extract::Request,
) -> Result<impl IntoResponse> {
	let query = request.uri().query().unwrap_or_default();
	let params: CompleteParams = serde_html_form::from_str(query)?;

	let Ok(oidc) = services.oauth.get_server() else {
		return Err!(Request(NotFound("OIDC server not configured")));
	};

	let user_id = services
		.users
		.find_from_login_token(&params.login_token)
		.await
		.map_err(|_| err!(Request(Forbidden("Invalid or expired login token"))))?;

	let auth_req = oidc
		.take_auth_request(&params.oidc_req_id)
		.await?;

	let code = oidc.create_auth_code(&auth_req, user_id);
	let redirect_url = Url::parse(&auth_req.redirect_uri)
		.map_err(|_| err!(Request(InvalidParam("Invalid redirect_uri"))))
		.map(|mut url| {
			url.query_pairs_mut().append_pair("code", &code);
			if let Some(state) = &auth_req.state {
				url.query_pairs_mut().append_pair("state", state);
			}
			url
		})?;

	Ok(Redirect::temporary(redirect_url.as_str()))
}
