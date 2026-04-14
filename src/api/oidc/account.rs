use axum::{
	extract::State,
	response::{IntoResponse, Response},
};
use http::{
	StatusCode,
	header::{CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY},
};
use futures::StreamExt;
use ruma::{OwnedDeviceId, OwnedRoomId, UserId};
use tuwunel_core::{Error, Result, err, info};

use super::url_encode;

/// Raw JS served at `/_tuwunel/oidc/account.js`; referenced via `<script src>` for CSP
/// compatibility.
const ACCOUNT_JS: &str =
	"document.querySelectorAll('time[data-ts]').forEach(function(el){\
	var t=+el.dataset.ts;if(t)el.textContent=new Date(t*1000).toLocaleString();});";

/// Shared stylesheet served at `/_tuwunel/oidc/account.css`.
const ACCOUNT_CSS: &str = "\
body{font-family:sans-serif;max-width:480px;margin:3em auto;padding:1em}\
body.wide{max-width:900px}\
a{color:#1976d2;text-decoration:none}\
a:hover{text-decoration:underline}\
table{border-collapse:collapse;width:100%}\
th,td{text-align:left;padding:.5em;border-bottom:1px solid #ddd}\
th{background:#f5f5f5}\
dl{display:grid;grid-template-columns:auto 1fr;gap:.4em 1em}\
dt{font-weight:bold}\
label{display:block;margin:.8em 0 .2em}\
input[type=text]{width:100%;padding:.4em;border:1px solid #ccc;border-radius:3px;\
font-size:1em;box-sizing:border-box}\
button{padding:.6em 1.4em;background:#1976d2;color:#fff;border:none;border-radius:4px;\
cursor:pointer;font-size:1em}\
button:hover{background:#1565c0}\
button.danger{background:#c62828}\
button.danger:hover{background:#b71c1c}\
.cancel{margin-left:1em;color:#1976d2;text-decoration:none}\
.actions,.nav{margin:2em 0}\
.actions a,.nav a{color:#1976d2;text-decoration:none;margin-right:1em}\
.actions a:hover,.nav a:hover{text-decoration:underline}\
.ok{color:#388e3c}\
.err{color:#c62828}\
.warn{color:#b71c1c}\
.meta{color:#666;font-size:.9em;margin:.4em 0}\
.center{text-align:center}\
.sep{color:#ccc}\
.submit-row{margin-top:1.2em}\
";

const ACCOUNT_HEAD: &str = "\
<meta charset=\"UTF-8\">\
<link rel=\"stylesheet\" href=\"/_tuwunel/oidc/account.css\">";

const ACCOUNT_JS_INCLUDE: &str = r#"<script src="/_tuwunel/oidc/account.js"></script>"#;
const ACCOUNT_CACHE_CONTROL: &str = "no-store";

/// CSP for account-management HTML pages.
///
/// The global CSP has `form-action 'none'` and `sandbox` (which both block form submission).
/// `SetResponseHeaderLayer::if_not_present` means our header takes precedence.
/// Styles are served from `/_tuwunel/oidc/account.css` so `style-src 'self'` suffices.
const ACCOUNT_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
                            form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

#[derive(Debug, Default, serde::Deserialize)]
struct AccountQueryParams {
	action:    Option<String>,
	device_id: Option<String>,
}

pub(crate) async fn account_route(
	State(services): State<crate::State>,
	request: axum::extract::Request,
) -> impl IntoResponse {
	let params: AccountQueryParams =
		match serde_html_form::from_str(request.uri().query().unwrap_or_default()) {
			| Ok(params) => params,
			| Err(e) => return account_error_response(&e.into()),
		};
	let action = params.action.as_deref().unwrap_or("org.matrix.sessions_list");
	let device_id = params.device_id.as_deref().unwrap_or("");

	match account_sso_redirect(&services, action, device_id) {
		| Ok(redirect) => account_redirect_response(redirect),
		| Err(e) => account_error_response(&e),
	}
}

pub(crate) async fn account_js_route() -> impl IntoResponse {
	(
		[
			(CONTENT_TYPE, "application/javascript; charset=utf-8"),
			// no-cache: revalidate on every request so a server update takes effect immediately
			(CACHE_CONTROL, "no-cache"),
		],
		ACCOUNT_JS,
	)
}

pub(crate) async fn account_css_route() -> impl IntoResponse {
	(
		[
			(CONTENT_TYPE, "text/css; charset=utf-8"),
			(CACHE_CONTROL, "no-cache"),
		],
		ACCOUNT_CSS,
	)
}

fn account_sso_redirect(
	services: &tuwunel_service::Services,
	action: &str,
	device_id: &str,
) -> Result<axum::response::Redirect> {
	validate_account_action(action)?;
	let default_idp = account_management_idp_id(services)?;
	let idp_id_enc = url_encode(&default_idp);

	let issuer = services.oauth.get_server()?.issuer_url()?;
	let base = issuer.trim_end_matches('/');

	let mut callback_url =
		url::Url::parse(&format!("{base}/_tuwunel/oidc/account_callback"))
			.map_err(|_| err!(error!("Failed to build account callback URL")))?;
	callback_url
		.query_pairs_mut()
		.append_pair("action", action)
		.append_pair("device_id", device_id);

	let mut sso_url =
		url::Url::parse(&format!("{base}/_matrix/client/v3/login/sso/redirect/{idp_id_enc}"))
			.map_err(|_| err!(error!("Failed to build SSO URL")))?;
	sso_url
		.query_pairs_mut()
		.append_pair("redirectUrl", callback_url.as_str());

	Ok(axum::response::Redirect::temporary(sso_url.as_str()))
}

#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct AccountCallbackParams {
	action:      Option<String>,
	device_id:   Option<String>,
	#[serde(rename = "loginToken")]
	login_token: Option<String>,
	displayname: Option<String>,
}

pub(crate) async fn account_callback_route(
	State(services): State<crate::State>,
	request: axum::extract::Request,
) -> impl IntoResponse {
	let params: AccountCallbackParams =
		match serde_html_form::from_str(request.uri().query().unwrap_or_default()) {
			| Ok(params) => params,
			| Err(e) => return account_error_response(&e.into()),
		};
	let html = match account_callback_inner(&services, params).await {
		| Ok(html) => html,
		| Err(e) => return account_error_response(&e),
	};
	account_html_response(StatusCode::OK, html)
}

pub(crate) async fn account_callback_post_route(
	State(services): State<crate::State>,
	axum::extract::Form(body): axum::extract::Form<AccountCallbackParams>,
) -> impl IntoResponse {
	let html = match account_callback_post_inner(&services, body).await {
		| Ok(html) => html,
		| Err(e) => return account_error_response(&e),
	};
	account_html_response(StatusCode::OK, html)
}

/// Consume a login token (single-use authentication).
async fn consume_login_token(
	services: &tuwunel_service::Services,
	token: Option<&str>,
) -> Result<ruma::OwnedUserId> {
	let token = token.ok_or_else(|| err!(Request(Forbidden("Missing login token"))))?;
	services
		.users
		.find_from_login_token(token)
		.await
		.map_err(|_| err!(Request(Forbidden("Invalid or expired login token"))))
}

/// Verify a login token without consuming it.
/// Used by GET handlers that embed the token in a POST confirmation form.
/// The token is consumed later when the form is submitted.
async fn peek_login_token(
	services: &tuwunel_service::Services,
	token: Option<&str>,
) -> Result<ruma::OwnedUserId> {
	let token = token.ok_or_else(|| err!(Request(Forbidden("Missing login token"))))?;
	services
		.users
		.peek_login_token(token)
		.await
		.map_err(|_| err!(Request(Forbidden("Invalid or expired login token"))))
}

async fn account_callback_inner(
	services: &tuwunel_service::Services,
	params: AccountCallbackParams,
) -> Result<String> {
	account_management_idp_id(services)?;
	let action = params.action.as_deref().unwrap_or("org.matrix.sessions_list");
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

	let login_token_str = login_token.unwrap_or("");

	match action {
		| "org.matrix.sessions_list" => sessions_list_html(services, &user_id).await,
		| "org.matrix.session_view" => {
			session_view_html(
				services,
				&user_id,
				params.device_id.as_deref().unwrap_or(""),
				login_token_str,
			)
			.await
		},
		| "org.matrix.session_end" => {
			// Authenticate first (peek), then show a POST confirmation form.
			// Actual deletion happens only on POST to prevent CSRF via GET.
			let device_id = params.device_id.as_deref().unwrap_or("");
			if device_id.is_empty() {
				return Err(err!(Request(InvalidParam("device_id is required"))));
			}
			let device_id_owned: OwnedDeviceId = device_id.into();
			if !services
				.users
				.device_exists(&user_id, &device_id_owned)
				.await
			{
				return Err(err!(Request(NotFound("Session not found"))));
			}
			Ok(session_end_confirm_html(&user_id, device_id, login_token_str))
		},
		| "org.matrix.profile" => profile_html(services, &user_id, login_token_str).await,
		| _ => Err(err!(Request(InvalidParam("Unsupported account management action")))),
	}
}

async fn account_callback_post_inner(
	services: &tuwunel_service::Services,
	body: AccountCallbackParams,
) -> Result<String> {
	account_management_idp_id(services)?;
	// Validate action before consuming the token so that an invalid action
	// does not burn the user's single-use login_token needlessly.
	let action = body.action.as_deref().unwrap_or("");
	validate_account_action(action)?;
	// Consume the token on POST — single-use, prevents replay.
	let user_id = consume_login_token(services, body.login_token.as_deref()).await?;

	match action {
		| "org.matrix.session_end" => {
			session_end_execute_html(services, &user_id, body.device_id.as_deref().unwrap_or(""))
				.await
		},
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
			let displayname = if cleaned_dn.is_empty() { None } else { Some(cleaned_dn.as_str()) };
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
		| _ => Err(err!(Request(InvalidParam("Unsupported POST action")))),
	}
}

async fn sessions_list_html(
	services: &tuwunel_service::Services,
	user_id: &UserId,
) -> Result<String> {
	let mut devices: Vec<_> = services
		.users
		.all_devices_metadata(user_id)
		.collect()
		.await;
	// Newest sessions first (highest last_seen_ts at top, None treated as oldest)
	devices.sort_by(|a, b| b.last_seen_ts.cmp(&a.last_seen_ts));
	let mut rows = String::new();
	for device in &devices {
		let name =
			html_escape(device.display_name.as_deref().unwrap_or("Unknown device"));
		let id = html_escape(device.device_id.as_str());
		let id_enc = url_encode(device.device_id.as_str());
		let ip = html_escape(device.last_seen_ip.as_deref().unwrap_or("—"));
		let ts_secs =
			device.last_seen_ts.map(|t| u64::from(t.as_secs())).unwrap_or(0);
		let ts_cell = ts_cell(ts_secs);
		rows.push_str(&format!(
			r#"<tr><td>{name}</td><td><code>{id}</code></td><td>{ip}</td><td>{ts_cell}</td>\
<td class="center">\
<a href="/_tuwunel/oidc/account?action=org.matrix.session_view&device_id={id_enc}">View</a>\
<span class="sep"> | </span>\
<a href="/_tuwunel/oidc/account?action=org.matrix.session_end&device_id={id_enc}" \
class="err">Sign out</a></td></tr>"#
		));
	}
	Ok(format!(
		r#"<!DOCTYPE html><html lang="en"><head>{ACCOUNT_HEAD}<title>Active Sessions</title></head>
<body class="wide"><h1>Active Sessions</h1>
<p>Signed in as <strong>{}</strong>. {} active session(s).</p>
<table><tr><th>Name</th><th>Device ID</th><th>Last seen IP</th><th>Last seen</th>\
<th class="center">Actions</th></tr>{rows}</table>
<div class="nav"><a href="/_tuwunel/oidc/account?action=org.matrix.profile">View Profile</a></div>
{ACCOUNT_JS_INCLUDE}
</body></html>"#,
		html_escape(user_id.as_str()),
		devices.len()
	))
}

async fn session_view_html(
	services: &tuwunel_service::Services,
	user_id: &UserId,
	device_id: &str,
	login_token: &str,
) -> Result<String> {
	if device_id.is_empty() {
		return Err(err!(Request(InvalidParam("device_id is required"))));
	}
	let device_id_owned: OwnedDeviceId = device_id.into();
	let device = services
		.users
		.get_device_metadata(user_id, &device_id_owned)
		.await
		.map_err(|_| err!(Request(NotFound("Session not found"))))?;
	let name = html_escape(device.display_name.as_deref().unwrap_or("Unknown device"));
	let id = html_escape(device.device_id.as_str());
	let id_enc = url_encode(device.device_id.as_str());
	let tok = html_escape(login_token);
	let ip = html_escape(device.last_seen_ip.as_deref().unwrap_or("—"));
	let ts_secs = device.last_seen_ts.map(|t| u64::from(t.as_secs())).unwrap_or(0);
	let ts_cell = ts_cell(ts_secs);
	// Link directly to account_callback (skips SSO) using the peeked login_token
	// so the user doesn't have to re-authenticate just to sign out a session.
	Ok(format!(
		r#"<!DOCTYPE html><html lang="en"><head>{ACCOUNT_HEAD}<title>Session: {name}</title></head>
<body><h1>Session Details</h1><p>Signed in as <strong>{}</strong>.</p>
<dl><dt>Name</dt><dd>{name}</dd><dt>Device ID</dt><dd><code>{id}</code></dd>\
<dt>Last seen IP</dt><dd>{ip}</dd><dt>Last seen</dt><dd>{ts_cell}</dd></dl>
<div class="actions">
  <a href="/_tuwunel/oidc/account?action=org.matrix.sessions_list">Back to sessions</a>
  <a href="/_tuwunel/oidc/account_callback?action=org.matrix.session_end&device_id={id_enc}&loginToken={tok}" \
class="err">Sign out this session</a>
</div>
{ACCOUNT_JS_INCLUDE}
</body></html>"#,
		html_escape(user_id.as_str())
	))
}

/// Shows a POST confirmation form. The `login_token` is the original SSO-issued token,
/// peeked (not consumed) by the GET handler and embedded here as the CSRF/auth token.
/// It is consumed when the user submits this form.
fn session_end_confirm_html(user_id: &UserId, device_id: &str, login_token: &str) -> String {
	let uid = html_escape(user_id.as_str());
	let did = html_escape(device_id);
	let did_enc = url_encode(device_id);
	let tok = html_escape(login_token);
	// url_encode for use in the Cancel href query parameter.
	let tok_enc = url_encode(login_token);
	format!(
		r#"<!DOCTYPE html><html lang="en"><head>{ACCOUNT_HEAD}<title>Sign Out Session</title></head>
<body><h1>Sign Out Session</h1>
<p>Signed in as <strong>{uid}</strong>.</p>
<p class="warn">Sign out session <code>{did}</code>? This will immediately invalidate its \
access token.</p>
<form method="POST" action="/_tuwunel/oidc/account_callback">
<input type="hidden" name="action" value="org.matrix.session_end">
<input type="hidden" name="device_id" value="{did}">
<input type="hidden" name="loginToken" value="{tok}">
<button type="submit" class="danger">Sign out</button>
<a class="cancel" href="/_tuwunel/oidc/account_callback?action=org.matrix.session_view&\
device_id={did_enc}&loginToken={tok_enc}">Cancel</a>
</form>
</body></html>"#
	)
}

/// Executes the actual session deletion. Called only from the POST handler.
async fn session_end_execute_html(
	services: &tuwunel_service::Services,
	user_id: &UserId,
	device_id: &str,
) -> Result<String> {
	if device_id.is_empty() {
		return Err(err!(Request(InvalidParam("device_id is required"))));
	}
	let device_id_owned: OwnedDeviceId = device_id.into();
	if !services
		.users
		.device_exists(user_id, &device_id_owned)
		.await
	{
		return Err(err!(Request(NotFound("Session not found"))));
	}
	services
		.users
		.remove_device(user_id, &device_id_owned)
		.await;
	info!("Session {device_id_owned} for {user_id} signed out via account management page");
	Ok(format!(
		r#"<!DOCTYPE html><html lang="en"><head>{ACCOUNT_HEAD}<title>Session Signed Out</title>\
</head>
<body><h1 class="ok">Session Signed Out</h1>
<p>Session <code>{}</code> for <strong>{}</strong> has been signed out.</p>
<div class="nav"><a href="/_tuwunel/oidc/account?action=org.matrix.sessions_list">\
Back to sessions</a></div>
</body></html>"#,
		html_escape(device_id_owned.as_str()),
		html_escape(user_id.as_str())
	))
}

async fn profile_html(
	services: &tuwunel_service::Services,
	user_id: &UserId,
	login_token: &str,
) -> Result<String> {
	let displayname = services.users.displayname(user_id).await.unwrap_or_default();
	let avatar_url = services
		.users
		.avatar_url(user_id)
		.await
		.ok()
		.map(|u| u.to_string())
		.unwrap_or_default();
	let server = html_escape(services.config.server_name.as_str());
	let uid = html_escape(user_id.as_str());
	let dn = html_escape(&displayname);
	let av = html_escape(&avatar_url);
	let tok = html_escape(login_token);
	let avatar_field = if av.is_empty() {
		String::new()
	} else {
		format!(
			r#"<p class="meta">Avatar: <code>{av}</code> (use your Matrix client to change)</p>"#
		)
	};
	Ok(format!(
		r#"<!DOCTYPE html><html lang="en"><head>{ACCOUNT_HEAD}<title>Profile</title></head>
<body><h1>Profile</h1><p>Signed in as <strong>{uid}</strong> on <strong>{server}</strong>.</p>
<form method="POST" action="/_tuwunel/oidc/account_callback">
<input type="hidden" name="action" value="org.matrix.profile">
<input type="hidden" name="loginToken" value="{tok}">
<label for="dn">Display name</label>
<input type="text" id="dn" name="displayname" value="{dn}" maxlength="255" \
autocomplete="name">
{avatar_field}
<p class="submit-row"><button type="submit">Save</button></p>
</form>
<div class="nav"><a href="/_tuwunel/oidc/account?action=org.matrix.sessions_list">\
Back to sessions</a></div>
</body></html>"#
	))
}

fn profile_saved_html(user_id: &UserId, displayname: Option<&str>) -> String {
	let uid = html_escape(user_id.as_str());
	let dn = html_escape(displayname.unwrap_or("(none)"));
	format!(
		r#"<!DOCTYPE html><html lang="en"><head>{ACCOUNT_HEAD}<title>Profile Saved</title></head>
<body><h1 class="ok">Profile Saved</h1>
<p>Display name for <strong>{uid}</strong> updated to: <strong>{dn}</strong>.</p>
<div class="nav">
  <a href="/_tuwunel/oidc/account?action=org.matrix.profile">Edit profile</a>
  <a href="/_tuwunel/oidc/account?action=org.matrix.sessions_list">Back to sessions</a>
</div>
</body></html>"#
	)
}

fn account_error_page(message: &str) -> String {
	let msg = html_escape(message);
	format!(
		r#"<!DOCTYPE html><html lang="en"><head>{ACCOUNT_HEAD}<title>Error</title></head>
<body><h1 class="err">Error</h1><p>{msg}</p>
<div class="nav"><a href="/_tuwunel/oidc/account">Return to account management</a></div>
</body></html>"#
	)
}

fn account_html_response(status: StatusCode, html: String) -> Response {
	(
		status,
		[
			(CONTENT_SECURITY_POLICY, ACCOUNT_CSP),
			(CACHE_CONTROL, ACCOUNT_CACHE_CONTROL),
			// Prevent the login token in the callback URL from leaking via
			// the Referer header to any embedded resources.
			(REFERRER_POLICY, "no-referrer"),
		],
		axum::response::Html(html),
	)
		.into_response()
}

fn account_error_response(error: &Error) -> Response {
	account_html_response(error.status_code(), account_error_page(&error.sanitized_message()))
}

fn account_redirect_response(redirect: axum::response::Redirect) -> Response {
	let mut response = redirect.into_response();
	response.headers_mut().insert(
		CACHE_CONTROL,
		http::HeaderValue::from_static(ACCOUNT_CACHE_CONTROL),
	);
	response.headers_mut().insert(
		REFERRER_POLICY,
		http::HeaderValue::from_static("no-referrer"),
	);
	response
}

fn account_management_idp_id(services: &tuwunel_service::Services) -> Result<String> {
	if services.config.identity_provider.len() != 1 {
		return Err(err!(Request(InvalidParam(
			"Account management requires exactly one configured identity provider"
		))));
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
		| _ => Err(err!(Request(InvalidParam(
			"Unsupported account management action"
		)))),
	}
}

fn ts_cell(ts_secs: u64) -> String {
	if ts_secs == 0 {
		"—".to_owned()
	} else {
		format!(r#"<time data-ts="{ts_secs}">—</time>"#)
	}
}

fn html_escape(s: &str) -> String {
	s.chars().fold(String::with_capacity(s.len()), |mut out, c| {
		match c {
			| '&' => out.push_str("&amp;"),
			| '<' => out.push_str("&lt;"),
			| '>' => out.push_str("&gt;"),
			| '"' => out.push_str("&quot;"),
			| '\'' => out.push_str("&#x27;"),
			| '`' => out.push_str("&#x60;"),
			| c => out.push(c),
		}
		out
	})
}
