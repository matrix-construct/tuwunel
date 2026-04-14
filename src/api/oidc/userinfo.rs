use axum::{
	Json, RequestPartsExt,
	body::Body,
	extract::State,
	http::Method,
	response::IntoResponse,
};
use axum_extra::{
	TypedHeader,
	headers::{Authorization, authorization::Bearer},
};
use futures::future::join;
use http::{Response, StatusCode};
use serde::Deserialize;
use serde_json::json;
use tuwunel_core::{Err, Result, utils::TryFutureExtExt};

use super::oauth_error;

#[derive(Deserialize)]
struct AccessTokenForm {
	access_token: Option<String>,
}

pub(crate) async fn userinfo_route(
	State(services): State<crate::State>,
	request: axum::extract::Request,
) -> Response<Body> {
	userinfo_inner(&services, request)
		.await
		.unwrap_or_else(|e| {
			let status = e.status_code();
			let mut resp = oauth_error(status, "invalid_token", &e.sanitized_message());
			// RFC 6750 §3: include WWW-Authenticate on 401 responses.
			if status == StatusCode::UNAUTHORIZED {
				resp.headers_mut().insert(
					http::header::WWW_AUTHENTICATE,
					http::HeaderValue::from_static(
						r#"Bearer realm="Matrix", error="invalid_token""#,
					),
				);
			}
			resp
		})
}

async fn userinfo_inner(
	services: &tuwunel_service::Services,
	request: axum::extract::Request,
) -> Result<Response<Body>> {
	let (mut parts, body) = request.into_parts();

	// Authorization header takes priority (required for GET, preferred for POST).
	let bearer: Option<TypedHeader<Authorization<Bearer>>> =
		parts.extract().await.unwrap_or(None);

	let token = if let Some(TypedHeader(Authorization(b))) = bearer {
		b.token().to_owned()
	} else if parts.method == Method::POST {
		// RFC 6750 §2.2: POST body may carry access_token as form parameter.
		let bytes = axum::body::to_bytes(body, 8192)
			.await
			.map_err(|_| tuwunel_core::err!(Request(BadJson("Failed to read request body"))))?;
		serde_html_form::from_bytes::<AccessTokenForm>(&bytes)
			.ok()
			.and_then(|f| f.access_token)
			.ok_or_else(|| tuwunel_core::err!(Request(MissingToken("No access token provided"))))?
	} else {
		return Err!(Request(MissingToken("No access token provided")));
	};

	let Ok((user_id, device_id, _expires)) = services.users.find_from_token(&token).await else {
		return Err!(Request(Unauthorized("Invalid access token")));
	};

	// RFC OIDC Core §5.3: the userinfo endpoint MUST only respond to tokens
	// that were issued through an OIDC flow (i.e. with the openid scope).
	// Reject plain Matrix access tokens that were not issued via OIDC.
	if !services.users.is_oidc_device(&user_id, &device_id).await {
		return Err!(Request(Unauthorized("Token was not issued through OIDC")));
	}

	let avatar_url = services.users.avatar_url(&user_id).ok();

	let displayname = services.users.displayname(&user_id).ok();

	let (avatar_url, displayname) = join(avatar_url, displayname).await;

	Ok(Json(json!({
		"sub": user_id.to_string(),
		"name": displayname,
		"picture": avatar_url,
	}))
	.into_response())
}
