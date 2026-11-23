#[cfg(test)]
mod tests;

mod account_deactivate;
mod cross_signing_reset;
mod profile;
mod profile_saved;
mod session_end_confirm;
mod session_end_execute;
mod session_list;
mod session_view;

use std::fmt;

use axum::{
	extract::{RawForm, Request, State},
	response::{Html, IntoResponse, Redirect, Response},
};
use futures::StreamExt;
use http::{
	HeaderValue, Method, StatusCode,
	header::{CACHE_CONTROL, CONTENT_TYPE, REFERRER_POLICY},
};
use itertools::Itertools;
use ruma::{DeviceId, OwnedDeviceId, UserId, profile::ProfileFieldName};
use serde_html_form::{from_bytes, from_str};
use tuwunel_api::router::ACCOUNT_HEAD;
use tuwunel_core::{
	Err, Error, Result, err,
	smallvec::SmallVec,
	utils::{
		BoolExt,
		html::escape as html_escape,
		stream::{IterStream, WidebandExt},
	},
};
use tuwunel_service::Services;
use url::Url;

use self::{
	account_deactivate::{account_deactivate_confirm_html, account_deactivate_execute_html},
	cross_signing_reset::{cross_signing_reset_confirm_html, cross_signing_reset_execute_html},
	profile::profile_html,
	profile_saved::profile_saved_html,
	session_end_confirm::session_end_confirm_html,
	session_end_execute::session_end_execute_html,
	session_list::sessions_list_html,
	session_view::session_view_html,
};
use super::{consume_login_token, peek_login_token, sso_redirect_url, url_encode};

pub(crate) static ACCOUNT_MANAGEMENT_ACTIONS_SUPPORTED: &[&str] = &[
	"org.matrix.profile",
	"org.matrix.devices_list",
	"org.matrix.device_view",
	"org.matrix.device_delete",
	"org.matrix.account_deactivate",
	"org.matrix.cross_signing_reset",
	"org.matrix.sessions_list",
	"org.matrix.session_view",
	"org.matrix.session_end",
];

/// Raw JS served at `/_tuwunel/oidc/account.js`.
/// Referenced via `<script src>` for CSP compatibility.
static ACCOUNT_JS: &str = include_str!("account/account.js");

/// Shared stylesheet served at `/_tuwunel/oidc/account.css`.
static ACCOUNT_CSS: &str = include_str!("account/account.css");

static ACCOUNT_JS_INCLUDE: &str = r#"
	<script src="/_tuwunel/oidc/account.js"></script>
"#;

/// Cache-control header value.
static ACCOUNT_CACHE_CONTROL: &str = "no-store";

/// Sessions named by repeated `device_id` parameters.
///
/// A link names one session; only the session list's sign-out form names more.
type DeviceIds = SmallVec<[OwnedDeviceId; 1]>;

/// The selected sessions that still exist.
///
/// Each borrows its device ID from the request's parameters.
type Sessions<'a> = SmallVec<[&'a DeviceId; 1]>;

#[derive(Debug, Default, serde::Deserialize)]
struct AccountQueryParams {
	action: Option<String>,
	device_id: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct AccountCallbackParams {
	action: Option<String>,
	#[serde(default, rename = "device_id")]
	device_ids: DeviceIds,
	#[serde(rename = "loginToken")]
	login_token: Option<String>,
	displayname: Option<String>,
}

pub(crate) async fn get_account_route(
	State(services): State<crate::State>,
	request: Request,
) -> impl IntoResponse {
	let params: AccountQueryParams = match from_str(request.uri().query().unwrap_or_default()) {
		| Err(e) => return account_error_response(&e.into()),
		| Ok(params) => params,
	};

	let action = params
		.action
		.as_deref()
		.unwrap_or("org.matrix.sessions_list");

	let device_id = params.device_id.as_deref().unwrap_or_default();

	match account_auth_redirect(&services, action, device_id) {
		| Ok(response) => response,
		| Err(e) => account_error_response(&e),
	}
}

fn account_auth_redirect(services: &Services, action: &str, device_id: &str) -> Result<Response> {
	validate_account_action(action)?;

	match services.config.authentication.oidc.native_auth {
		| true => account_native_redirect(services, action, device_id),
		| false => {
			let idp_id = services.oauth.providers.get_default_id();

			account_sso_redirect(services, action, device_id, idp_id.as_deref())
		},
	}
}

fn account_native_redirect(
	services: &Services,
	action: &str,
	device_id: &str,
) -> Result<Response> {
	let issuer = services.oauth.get_server()?.issuer_url()?;
	let base = issuer.trim_end_matches('/');

	let native_url = Url::parse_with_params(&format!("{base}/_tuwunel/oidc/native"), [
		("action", action),
		("device_id", device_id),
	])
	.map_err(|_| err!(Request(InvalidParam("Failed to build native login URL"))))?;

	Ok(account_redirect_response(Redirect::temporary(native_url.as_str())))
}

/// Redirect account authentication through the selected identity provider.
///
/// The callback preserves the requested action and device selector.
pub(super) fn account_sso_redirect(
	services: &Services,
	action: &str,
	device_id: &str,
	idp_id: Option<&str>,
) -> Result<Response> {
	let idp_id = idp_id
		.ok_or_else(|| err!(Config("identity_provider", "No identity provider configured")))?;

	let issuer = services.oauth.get_server()?.issuer_url()?;
	let base = issuer.trim_end_matches('/');

	let callback_url =
		Url::parse_with_params(&format!("{base}/_tuwunel/oidc/account_callback"), [
			("action", action),
			("device_id", device_id),
		])
		.map_err(|_| err!(error!("Failed to build account callback URL")))?;

	let sso_url = sso_redirect_url(base, idp_id, &callback_url)?;

	Ok(account_redirect_response(Redirect::temporary(sso_url.as_str())))
}

pub(crate) async fn get_account_callback_route(
	State(services): State<crate::State>,
	request: Request,
) -> impl IntoResponse {
	let query = request.uri().query().unwrap_or_default();

	account_callback_response(&services, Method::GET, query.as_bytes()).await
}

pub(crate) async fn post_account_callback_route(
	State(services): State<crate::State>,
	RawForm(body): RawForm,
) -> impl IntoResponse {
	account_callback_response(&services, Method::POST, &body).await
}

async fn account_callback_response(services: &Services, method: Method, form: &[u8]) -> Response {
	match handle_account_callback(services, method, form).await {
		| Ok(html) => account_html_response(StatusCode::OK, html),
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

async fn handle_account_callback(
	services: &Services,
	method: Method,
	form: &[u8],
) -> Result<String> {
	// Repeated keys parse, since a selection of sessions repeats `device_id`.
	let params: AccountCallbackParams = from_bytes(form)
		.map_err(|e| err!(Request(InvalidParam("Invalid account management parameters: {e}"))))?;

	let login_token = params.login_token.as_deref();

	let fallback_action = method
		.eq(&Method::GET)
		.then_some("org.matrix.sessions_list");

	let action = params
		.action
		.as_deref()
		.or(fallback_action)
		.unwrap_or_default();

	// Validations before consuming the token so that an invalid action does not
	// burn the user's single-use login_token needlessly.
	services.oauth.get_server()?;

	let has_provider = services.config.authentication.oidc.native_auth
		|| services
			.oauth
			.providers
			.get_default_id()
			.is_some();

	has_provider.then_some(()).ok_or_else(|| {
		err!(Config(
			"identity_provider",
			"No identity provider or native authentication configured"
		))
	})?;

	validate_account_action(action)?;

	// MSC4191 stable action names dispatch through the prototype aliases.
	let action = normalize_account_action(action);

	// A GET peeks so its page can embed the token, and a POST consumes it; a sign-out
	// consumes only once its selection resolves, so a refused one keeps the token.
	let user_id = match method {
		| Method::POST if action != "org.matrix.session_end" =>
			consume_login_token(services, login_token).await?,
		| Method::GET | Method::POST => peek_login_token(services, login_token).await?,
		| _ =>
			return Err!(HttpJson(METHOD_NOT_ALLOWED, {
				"errcode": "M_UNRECOGNIZED",
				"error": "Unsupported account management method",
			})),
	};

	let login_token = login_token.unwrap_or_default();

	match action {
		| "org.matrix.sessions_list" if method == Method::GET =>
			sessions_list_html(services, &user_id, login_token).await,

		| "org.matrix.profile" if method == Method::GET =>
			profile_html(services, &user_id, login_token).await,

		| "org.matrix.profile" if method == Method::POST => {
			if !may_set_displayname(services, &user_id).await {
				return Err!(Request(Forbidden("Setting the display name has been disabled.")));
			}

			// Sanitize: strip control chars, limit to 255 Unicode code points.
			let cleaned_dn: String = params
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

			services
				.profile
				.set_displayname(&user_id, displayname, None)
				.await?;

			profile_saved_html(&user_id, displayname).await
		},

		| "org.matrix.session_end" if method == Method::POST =>
			session_end_execute_html(services, &user_id, &params.device_ids, login_token).await,

		// Deletion happens only on the confirming POST, so a GET cannot forge it.
		| "org.matrix.session_end" if method == Method::GET =>
			session_end_confirm_html(services, &user_id, &params.device_ids, login_token).await,

		| "org.matrix.account_deactivate" if method == Method::POST =>
			account_deactivate_execute_html(services, &user_id).await,

		| "org.matrix.account_deactivate" if method == Method::GET =>
			account_deactivate_confirm_html(&user_id, login_token).await,

		| "org.matrix.cross_signing_reset" if method == Method::POST =>
			cross_signing_reset_execute_html(services, &user_id).await,

		| "org.matrix.cross_signing_reset" if method == Method::GET =>
			cross_signing_reset_confirm_html(&user_id, login_token).await,

		| "org.matrix.session_view" if method == Method::GET => {
			let device_id = params
				.device_ids
				.first()
				.map(OwnedDeviceId::as_ref)
				.map(DeviceId::as_str)
				.unwrap_or_default();

			session_view_html(services, &user_id, device_id, login_token).await
		},

		| _ => Err!(Request(InvalidParam("Unsupported account management action"))),
	}
}

/// Whether the user may change their display name on the profile page.
///
/// A login-token session is never an appservice's, so only the admin exemption
/// can lift `profile.enable_set_displayname`. The page shows its form and
/// accepts a save on the same answer.
async fn may_set_displayname(services: &Services, user_id: &UserId) -> bool {
	let is_admin = || services.admin.user_is_admin(user_id);

	services
		.profile
		.may_set_field(&ProfileFieldName::DisplayName, false, is_admin)
		.await
}

pub(super) fn account_redirect_response(redirect: Redirect) -> Response {
	let mut response = redirect.into_response();

	response
		.headers_mut()
		.insert(CACHE_CONTROL, HeaderValue::from_static(ACCOUNT_CACHE_CONTROL));

	response
		.headers_mut()
		.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));

	response
}

// Prevent the login token in the callback URL from leaking via the Referer
// header to any embedded resources.
pub(super) fn account_html_response(status: StatusCode, html: String) -> Response {
	let headers = [(CACHE_CONTROL, ACCOUNT_CACHE_CONTROL), (REFERRER_POLICY, "no-referrer")];

	(status, headers, Html(html)).into_response()
}

pub(super) fn account_error_response(error: &Error) -> Response {
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

/// Reject unsupported account management actions before authentication.
///
/// Unsupported actions return an invalid-parameter error.
pub(super) fn validate_account_action(action: &str) -> Result {
	ACCOUNT_MANAGEMENT_ACTIONS_SUPPORTED
		.contains(&action)
		.ok_or_else(|| err!(Request(InvalidParam("Unsupported account management action"))))
}

fn normalize_account_action(action: &str) -> &str {
	match action {
		| "org.matrix.devices_list" => "org.matrix.sessions_list",
		| "org.matrix.device_view" => "org.matrix.session_view",
		| "org.matrix.device_delete" => "org.matrix.session_end",
		| other => other,
	}
}

/// Resolves a selection to the sessions that still exist.
///
/// The result is sorted and free of repeats. An empty selection is an invalid
/// parameter, and a selection whose sessions are all gone is not found.
async fn selected_sessions<'a>(
	services: &Services,
	user_id: &UserId,
	device_ids: &'a [OwnedDeviceId],
) -> Result<Sessions<'a>> {
	let requested = device_ids
		.iter()
		.map(OwnedDeviceId::as_ref)
		.filter(|device_id: &&DeviceId| device_id.as_str().is_empty().is_false());

	if requested.clone().next().is_none() {
		return Err!(Request(InvalidParam("Select at least one session")));
	}

	let sessions: Sessions<'a> = requested
		.sorted_unstable()
		.dedup()
		.stream()
		.wide_filter_map(async |device_id| {
			services
				.users
				.device_exists(user_id, device_id)
				.await
				.then_some(device_id)
		})
		.collect()
		.await;

	if sessions.is_empty() {
		return Err!(Request(NotFound("Session not found")));
	}

	Ok(sessions)
}

/// Renders an item per selected session from its escaped device ID.
///
/// Each page supplies the item's markup; the confirmation form's items also
/// carry the ID in a hidden input.
fn session_items(
	sessions: &[&DeviceId],
	item: impl Fn(&mut String, &str) -> fmt::Result,
) -> String {
	sessions
		.iter()
		.copied()
		.map(DeviceId::as_str)
		.map(html_escape)
		.fold(String::new(), |mut out, escaped| {
			item(&mut out, &escaped).ok();

			out
		})
}

fn ts_cell(ts_secs: u64) -> String {
	if ts_secs == 0 {
		return "—".to_owned();
	}

	format!(r#"<time data-ts="{ts_secs}">—</time>"#)
}
