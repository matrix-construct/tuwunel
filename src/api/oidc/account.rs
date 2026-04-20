mod profile;
mod profile_saved;
mod session_end_confirm;
mod session_end_execute;
mod session_list;
mod session_view;

use axum::{
	extract::{Form, Request, State},
	response::{Html, IntoResponse, Redirect, Response},
};
use futures::StreamExt;
use http::{
	HeaderValue, StatusCode,
	header::{CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY},
};
use ruma::{OwnedDeviceId, OwnedRoomId};
use tuwunel_core::{
	Err, Error, Result, err,
	utils::{BoolExt, html::escape as html_escape},
};
use tuwunel_service::Services;
use url::Url;

use self::{
	profile::profile_html, profile_saved::profile_saved_html,
	session_end_confirm::session_end_confirm_html, session_end_execute::session_end_execute_html,
	session_list::sessions_list_html, session_view::session_view_html,
};
use super::url_encode;

/// Raw JS served at `/_tuwunel/oidc/account.js`.
/// Referenced via `<script src>` for CSP compatibility.
static ACCOUNT_JS: &str = include_str!("account/account.js");

/// Shared stylesheet served at `/_tuwunel/oidc/account.css`.
static ACCOUNT_CSS: &str = include_str!("account/account.css");

static ACCOUNT_HEAD: &str = r#"
	<meta charset="UTF-8">
	<link rel="stylesheet" href="/_tuwunel/oidc/account.css">
"#;

static ACCOUNT_JS_INCLUDE: &str = r#"
	<script src="/_tuwunel/oidc/account.js"></script>
"#;

/// Cache-control header value.
static ACCOUNT_CACHE_CONTROL: &str = "no-store";

/// CSP for account-management HTML pages. The global CSP has `form-action
/// 'none'` and `sandbox` (which both block form submission).
/// `SetResponseHeaderLayer::if_not_present` means our header takes precedence.
/// Styles are served from `/_tuwunel/oidc/account.css` so `style-src 'self'`
/// suffices.
static ACCOUNT_CSP: &[&str] = &[
	"default-src 'none';",
	"script-src 'self';",
	"style-src 'self';",
	"form-action 'self';",
	"frame-ancestors 'none';",
	"base-uri 'none';",
];

#[derive(Debug, Default, serde::Deserialize)]
struct AccountQueryParams {
	action: Option<String>,
	device_id: Option<String>,
}

pub(crate) async fn account_route(
	State(services): State<crate::State>,
	request: Request,
) -> impl IntoResponse {
	let params: AccountQueryParams =
		match serde_html_form::from_str(request.uri().query().unwrap_or_default()) {
			| Err(e) => return account_error_response(&e.into()),
			| Ok(params) => params,
		};

	let action = params
		.action
		.as_deref()
		.unwrap_or("org.matrix.sessions_list");

	let device_id = params.device_id.as_deref().unwrap_or("");

	match account_sso_redirect(&services, action, device_id) {
		| Ok(redirect) => account_redirect_response(redirect),
		| Err(e) => account_error_response(&e),
	}
}

// no-cache: revalidate on every request so a server update takes effect
// immediately
pub(crate) async fn account_js_route() -> impl IntoResponse {
	let content_type = (CONTENT_TYPE, "application/javascript; charset=utf-8");
	let cache_control = (CACHE_CONTROL, "no-cache");

	([content_type, cache_control], ACCOUNT_JS)
}

pub(crate) async fn account_css_route() -> impl IntoResponse {
	let content_type = (CONTENT_TYPE, "text/css; charset=utf-8");
	let cache_control = (CACHE_CONTROL, "no-cache");

	([content_type, cache_control], ACCOUNT_CSS)
}

fn account_sso_redirect(services: &Services, action: &str, device_id: &str) -> Result<Redirect> {
	validate_account_action(action)?;

	let default_idp = account_management_idp_id(services)?;
	let idp_id_enc = url_encode(&default_idp);

	let issuer = services.oauth.get_server()?.issuer_url()?;
	let base = issuer.trim_end_matches('/');

	let mut callback_url = Url::parse(&format!("{base}/_tuwunel/oidc/account_callback"))
		.map_err(|_| err!(error!("Failed to build account callback URL")))?;

	callback_url
		.query_pairs_mut()
		.append_pair("action", action)
		.append_pair("device_id", device_id);

	let mut sso_url =
		Url::parse(&format!("{base}/_matrix/client/v3/login/sso/redirect/{idp_id_enc}"))
			.map_err(|_| err!(error!("Failed to build SSO URL")))?;

	sso_url
		.query_pairs_mut()
		.append_pair("redirectUrl", callback_url.as_str());

	Ok(Redirect::temporary(sso_url.as_str()))
}

#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct AccountCallbackParams {
	action: Option<String>,
	device_id: Option<String>,
	#[serde(rename = "loginToken")]
	login_token: Option<String>,
	displayname: Option<String>,
}

pub(crate) async fn account_callback_route(
	State(services): State<crate::State>,
	request: Request,
) -> impl IntoResponse {
	let params: AccountCallbackParams =
		match serde_html_form::from_str(request.uri().query().unwrap_or_default()) {
			| Err(e) => return account_error_response(&e.into()),
			| Ok(params) => params,
		};

	let html = match account_callback_inner(&services, params).await {
		| Err(e) => return account_error_response(&e),
		| Ok(html) => html,
	};

	account_html_response(StatusCode::OK, html)
}

pub(crate) async fn account_callback_post_route(
	State(services): State<crate::State>,
	Form(body): Form<AccountCallbackParams>,
) -> impl IntoResponse {
	let html = match account_callback_post_inner(&services, body).await {
		| Err(e) => return account_error_response(&e),
		| Ok(html) => html,
	};

	account_html_response(StatusCode::OK, html)
}

async fn account_callback_inner(
	services: &Services,
	params: AccountCallbackParams,
) -> Result<String> {
	account_management_idp_id(services)?;

	let action = params
		.action
		.as_deref()
		.unwrap_or("org.matrix.sessions_list");

	validate_account_action(action)?;

	let login_token = params.login_token.as_deref();

	// Read-only pages consume the token immediately.
	// Pages with a POST confirmation step peek at the token so it can be
	// embedded in the form and consumed only when the user confirms the action.
	// This avoids creating a second short-lived token on every GET, preventing
	// accumulation of orphaned tokens when the user navigates back.
	// sessions_list: read-only, consumes the token immediately.
	// session_view: read-only display, but has a "Sign out" link that POSTs later —
	//   use peek so the same token can be submitted in the confirmation form.
	// session_end / profile: confirmation-form flow, use peek (consumed on POST).
	let user_id = match action {
		| "org.matrix.sessions_list" => consume_login_token(services, login_token).await?,
		| _ => peek_login_token(services, login_token).await?,
	};

	match action {
		| "org.matrix.profile" =>
			profile_html(services, &user_id, login_token.unwrap_or_default()).await,

		| "org.matrix.sessions_list" => sessions_list_html(services, &user_id).await,

		| "org.matrix.session_view" =>
			session_view_html(
				services,
				&user_id,
				params.device_id.as_deref().unwrap_or_default(),
				login_token.unwrap_or_default(),
			)
			.await,

		| "org.matrix.session_end" => {
			// Authenticate first (peek), then show a POST confirmation form.
			// Actual deletion happens only on POST to prevent CSRF via GET.
			let device_id = params.device_id.clone().unwrap_or_default();
			if device_id.is_empty() {
				return Err!(Request(InvalidParam("device_id is required")));
			}

			let device_id_owned: OwnedDeviceId = device_id.into();
			if !services
				.users
				.device_exists(&user_id, &device_id_owned)
				.await
			{
				return Err!(Request(NotFound("Session not found")));
			}

			Ok(session_end_confirm_html(
				&user_id,
				device_id_owned.as_str(),
				login_token.unwrap_or_default(),
			))
		},
		| _ => Err!(Request(InvalidParam("Unsupported account management action"))),
	}
}

async fn account_callback_post_inner(
	services: &Services,
	body: AccountCallbackParams,
) -> Result<String> {
	account_management_idp_id(services)?;

	// Validate action before consuming the token so that an invalid action
	// does not burn the user's single-use login_token needlessly.
	let action = body.action.as_deref().unwrap_or_default();

	validate_account_action(action)?;

	// Consume the token on POST — single-use, prevents replay.
	let user_id = consume_login_token(services, body.login_token.as_deref()).await?;

	match action {
		| "org.matrix.session_end" =>
			session_end_execute_html(
				services,
				&user_id,
				body.device_id.as_deref().unwrap_or_default(),
			)
			.await,

		| "org.matrix.profile" => {
			// Sanitize: strip control chars, limit to 255 Unicode code points.
			let cleaned_dn: String = body
				.displayname
				.as_deref()
				.unwrap_or("")
				.trim()
				.chars()
				.filter(|c| !c.is_control())
				.take(255)
				.collect();

			let displayname = cleaned_dn
				.is_empty()
				.is_false()
				.then_some(cleaned_dn.as_str());

			let all_joined_rooms: Vec<OwnedRoomId> = services
				.state_cache
				.rooms_joined(&user_id)
				.map(ToOwned::to_owned)
				.collect()
				.await;

			services
				.users
				.update_displayname(&user_id, displayname, &all_joined_rooms)
				.await;

			Ok(profile_saved_html(&user_id, displayname))
		},
		| _ => Err!(Request(InvalidParam("Unsupported POST action"))),
	}
}

fn account_redirect_response(redirect: Redirect) -> Response {
	let mut response = redirect.into_response();

	response
		.headers_mut()
		.insert(CACHE_CONTROL, HeaderValue::from_static(ACCOUNT_CACHE_CONTROL));

	response
		.headers_mut()
		.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));

	response
}

// Prevent the login token in the callback URL from leaking via
// the Referer header to any embedded resources.
fn account_html_response(status: StatusCode, html: String) -> Response {
	let csp = ACCOUNT_CSP.join("");
	let headers = [
		(CACHE_CONTROL, ACCOUNT_CACHE_CONTROL),
		(CONTENT_SECURITY_POLICY, csp.as_str()),
		(REFERRER_POLICY, "no-referrer"),
	];

	(status, headers, Html(html)).into_response()
}

fn account_error_response(error: &Error) -> Response {
	let msg = error.sanitized_message();
	let code = error.status_code();

	account_html_response(code, account_error_page(&msg))
}

fn account_error_page(message: &str) -> String {
	let msg = html_escape(message);

	format!(
		r#"<!DOCTYPE html>
		<html lang="en">
			<head>
				{ACCOUNT_HEAD}
				<title>Error</title>
			</head>
			<body>
				<h1 class="err">Error</h1>
				<p>{msg}</p>
				<div class="nav">
					<a href="/_tuwunel/oidc/account">
						Return to account management
					</a>
				</div>
			</body>
		</html>"#
	)
}

/// Consume a login token (single-use authentication).
async fn consume_login_token(
	services: &Services,
	token: Option<&str>,
) -> Result<ruma::OwnedUserId> {
	let token = token.ok_or(err!(Request(Forbidden("Missing login token"))))?;

	services
		.users
		.find_from_login_token(token)
		.await
		.map_err(|_| err!(Request(Forbidden("Invalid or expired login token"))))
}

/// Verify a login token without consuming it.
/// Used by GET handlers that embed the token in a POST confirmation form.
/// The token is consumed later when the form is submitted.
async fn peek_login_token(services: &Services, token: Option<&str>) -> Result<ruma::OwnedUserId> {
	let token = token.ok_or(err!(Request(Forbidden("Missing login token"))))?;

	services
		.users
		.peek_login_token(token)
		.await
		.map_err(|_| err!(Request(Forbidden("Invalid or expired login token"))))
}

fn account_management_idp_id(services: &Services) -> Result<String> {
	if services.config.identity_provider.len() != 1 {
		return Err!(Request(InvalidParam(
			"Account management requires exactly one configured identity provider"
		)));
	}

	services
		.oauth
		.providers
		.get_default_id()
		.ok_or_else(|| err!(Config("identity_provider", "No identity provider configured")))
}

fn validate_account_action(action: &str) -> Result {
	match action {
		| "org.matrix.profile"
		| "org.matrix.sessions_list"
		| "org.matrix.session_view"
		| "org.matrix.session_end" => Ok(()),
		| _ => Err!(Request(InvalidParam("Unsupported account management action"))),
	}
}

fn ts_cell(ts_secs: u64) -> String {
	if ts_secs == 0 {
		return "—".to_owned();
	}

	format!(r#"<time data-ts="{ts_secs}">—</time>"#)
}
